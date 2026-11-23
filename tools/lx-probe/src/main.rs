use std::env;

use anyhow::{Context, Result, bail};
use lx_probe::{Session, read_input};
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--worker"] {
        return lx_probe::run_worker();
    }
    let full_url = args.first().is_some_and(|arg| arg == "--full-url");
    let [script, actions] = &args[usize::from(full_url)..] else {
        bail!("usage: lx-probe [--full-url] SCRIPT.js REQUESTS.json")
    };
    let script = read_input(script)?;
    let actions: Vec<Value> = serde_json::from_str(&read_input(actions)?)?;

    let mut session = Session::initialize_direct(script, "--worker").await?;
    println!("{}", json!({"id":0,"sources":session.sources}));
    for (index, action) in actions.iter().enumerate() {
        let result = session.action(action).await?;
        let id = index + 1;
        let record = if action["action"] == "lyric" {
            json!({"id": id, "lyric":result})
        } else if full_url {
            json!({"id":id, "url":result})
        } else {
            json!({"id":id, "origin":reqwest::Url::parse(result.as_str().context("expected URL")?)?.origin().ascii_serialization()})
        };
        println!("{record}");
    }
    Ok(())
}
