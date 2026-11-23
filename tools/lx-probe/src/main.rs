mod http;
mod worker;

use std::{collections::HashMap, env, fs::File, io::Read, process::Stdio, time::Duration};

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

const FRAME_LIMIT: usize = 4 * 1024 * 1024;
const SCRIPT_LIMIT: usize = 1024 * 1024;

fn read_input(path: &str) -> Result<String> {
    let mut raw = Vec::new();
    File::open(path)?.take(SCRIPT_LIMIT as u64 + 1).read_to_end(&mut raw)?;
    ensure!(raw.len() <= SCRIPT_LIMIT, "input exceeds 1 MiB");
    String::from_utf8(raw).context("input is not valid UTF-8")
}

type Job = BoxFuture<'static, (u64, Value)>;

struct Session {
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

    async fn probe(&mut self, script: String, actions: &[Value], full_url: bool) -> Result<()> {
        let sources = self
            .phase(json!({"event":"init","id":0,"script":script}))
            .await
            .context("initialization")?;
        println!("{}", json!({"id":0,"sources":sources}));
        for (index, action) in actions.iter().enumerate() {
            let id = index + 1;
            let source = action["source"].as_str().context("missing source")?;
            let quality = action["info"]["type"].as_str().context("missing quality")?;
            let supports = |key: &str, target: &str| {
                sources[source][key].as_array().is_some_and(|xs| xs.iter().any(|x| x == target))
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
            let url = http::http_url(&result)?;
            let record = if full_url {
                json!({"id": id, "url": result})
            } else {
                json!({"id": id, "origin": url.origin().ascii_serialization()})
            };
            println!("{record}");
        }
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--worker"] {
        return worker::run();
    }
    let full_url = args.first().is_some_and(|arg| arg == "--full-url");
    let [script, actions] = &args[usize::from(full_url)..] else {
        bail!("usage: lx-probe [--full-url] SCRIPT.js REQUESTS.json")
    };
    let script = read_input(script)?;
    let actions: Vec<Value> = serde_json::from_str(&read_input(actions)?)?;
    let mut child = Command::new(env::current_exe()?)
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let codec = || LinesCodec::new_with_max_length(FRAME_LIMIT);
    let mut session = Session {
        input: FramedWrite::new(child.stdin.take().context("worker stdin")?, codec()),
        output: FramedRead::new(child.stdout.take().context("worker stdout")?, codec()),
        client: Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build()?,
        jobs: FuturesUnordered::new(),
        pending: HashMap::new(),
    };
    let outcome = session.probe(script, &actions, full_url).await;
    drop(session);
    child.kill().await?;
    outcome
}
