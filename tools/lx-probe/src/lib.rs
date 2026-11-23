mod http;
mod worker;
use std::{
    collections::HashMap, env, fs::File, io::Read, path::Path, process::Stdio, time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{
    FutureExt, SinkExt, StreamExt,
    future::{AbortHandle, Abortable, BoxFuture},
    stream::FuturesUnordered,
};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::{
    process::{ChildStdin, ChildStdout, Command},
    time::{sleep, timeout},
};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
pub use worker::run as run_worker;

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const SCRIPT_LIMIT: usize = 1024 * 1024;

pub fn read_input(path: impl AsRef<Path>) -> Result<String> {
    let mut raw = Vec::new();
    File::open(path)?.take(SCRIPT_LIMIT as u64 + 1).read_to_end(&mut raw)?;
    ensure!(raw.len() <= SCRIPT_LIMIT, "input exceeds 1 MiB");
    String::from_utf8(raw).context("input is not valid UTF-8")
}

type Job = BoxFuture<'static, (u64, Value)>;

pub struct Session {
    _child: tokio::process::Child,
    pub sources: Value,
    pub metadata: Value,
    next_id: u64,
    input: FramedWrite<ChildStdin, LinesCodec>,
    output: FramedRead<ChildStdout, LinesCodec>,
    jobs: FuturesUnordered<Job>,
    pending: HashMap<u64, AbortHandle>,
    client: Client,
}

impl Session {
    async fn send(&mut self, frame: Value) -> Result<()> {
        let data = serde_json::to_string(&frame)?;
        ensure!(data.len() <= FRAME_LIMIT, "oversize IPC frame");
        Ok(self.input.send(data).await?)
    }

    fn start_job(&mut self, id: u64, msg: Value) -> Result<()> {
        ensure!(
            self.pending.len() < 32 && !self.pending.contains_key(&id),
            "invalid pending callback"
        );
        let (abort, registration) = AbortHandle::new_pair();
        let client = self.client.clone();
        let task = async move {
            if msg["event"] == "timer" {
                sleep(Duration::from_millis(msg["ms"].as_u64().unwrap_or(0).min(i32::MAX as u64)))
                    .await;
                Ok(Value::Null)
            } else {
                http::request(&client, &msg).await
            }
        };
        self.pending.insert(id, abort);
        self.jobs.push(
            async move {
                let frame = match Abortable::new(task, registration).await {
                    Ok(Ok(response)) => json!({"response":response}),
                    Ok(Err(err)) => json!({"error":err.to_string()}),
                    Err(_) => json!({"error":"request cancelled"}),
                };
                (id, frame)
            }
            .boxed(),
        );
        Ok(())
    }

    async fn exchange(&mut self, command: Value) -> Result<Value> {
        let expected = command["id"].as_u64().context("missing command id")?;
        self.send(command).await?;
        loop {
            tokio::select! {
                biased;
                Some((id, mut reply)) = self.jobs.next(), if !self.jobs.is_empty() => {
                    self.pending.remove(&id);
                    reply["event"] = json!("callback");
                    reply["id"] = json!(id);
                    self.send(reply).await?;
                }
                frame = self.output.next() => {
                    let frame = frame.context("script host exited")??;
                    let msg: Value = serde_json::from_str(&frame)?;
                    let id = msg["id"].as_u64().context("missing event id")?;
                    match msg["event"].as_str() {
                        Some("result") if id == expected => return Ok(msg["value"].clone()),
                        Some("error") if id == expected => bail!("{}", msg["error"].as_str().unwrap_or("script error")),
                        Some("http" | "timer") => self.start_job(id, msg)?,
                        Some("cancel") => if let Some(handle) = self.pending.get(&id) { handle.abort(); },
                        _ => bail!("unexpected host event"),
                    }
                }
            }
        }
    }

    async fn phase(&mut self, command: Value) -> Result<Value> {
        timeout(Duration::from_secs(12), self.exchange(command))
            .await
            .context("script host deadline exceeded")?
    }

    pub async fn initialize(script: String, worker_arg: &str) -> Result<Self> {
        let client = Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
        Self::initialize_with_client(script, worker_arg, client).await
    }

    pub async fn initialize_direct(script: String, worker_arg: &str) -> Result<Self> {
        let client =
            Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build()?;
        Self::initialize_with_client(script, worker_arg, client).await
    }

    async fn initialize_with_client(
        script: String,
        worker_arg: &str,
        client: Client,
    ) -> Result<Self> {
        let mut child = Command::new(env::current_exe()?)
            .arg(worker_arg)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let codec = || LinesCodec::new_with_max_length(FRAME_LIMIT);
        let mut session = Self {
            input: FramedWrite::new(child.stdin.take().context("worker stdin")?, codec()),
            output: FramedRead::new(child.stdout.take().context("worker stdout")?, codec()),
            client,
            _child: child,
            sources: Value::Null,
            metadata: Value::Null,
            next_id: 1,
            jobs: FuturesUnordered::new(),
            pending: HashMap::new(),
        };

        let info = session
            .phase(json!({"event":"init","id":0,"script":script}))
            .await
            .context("initialization")?;
        session.sources = info["sources"].clone();
        session.metadata = info["metadata"].clone();
        Ok(session)
    }

    pub async fn resolve(&mut self, action: &Value) -> Result<String> {
        let id = self.next_id;
        self.next_id += 1;
        let source = action["source"].as_str().context("missing source")?;
        let quality = action["info"]["type"].as_str().context("missing quality")?;
        let supports = |key: &str, target: &str| {
            self.sources[source][key].as_array().is_some_and(|xs| xs.iter().any(|x| x == target))
        };
        ensure!(
            action["action"] == "musicUrl"
                && supports("actions", "musicUrl")
                && supports("qualitys", quality),
            "undeclared {source}/musicUrl/{quality}"
        );
        let result = self
            .phase(json!({"event":"action","id":id,"data":action}))
            .await
            .with_context(|| format!("action {id} ({source}/{quality})"))?;
        http::http_url(&result)?;
        Ok(serde_json::from_value(result)?)
    }
}
