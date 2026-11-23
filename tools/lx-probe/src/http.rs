use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, Url, header};
use serde_json::{Map, Value, json};

pub const BODY_LIMIT: usize = 512 * 1024;

pub fn http_url(value: &Value) -> Result<Url> {
    let url = Url::parse(value.as_str().ok_or_else(|| anyhow!("missing URL"))?)?;
    ensure!(matches!(url.scheme(), "http" | "https"), "expected HTTP(S) URL");
    Ok(url)
}

fn transport_error(err: reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        return anyhow!("HTTP timeout");
    }
    let err = err.without_url();
    let mut cause = &err as &dyn std::error::Error;
    while let Some(inner) = cause.source() {
        cause = inner;
    }
    anyhow!("HTTP transport: {cause}")
}

fn is_json_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    })
}

pub async fn request(client: &Client, msg: &Value) -> Result<Value> {
    let options = &msg["options"];
    let method_str = options["method"].as_str().unwrap_or("get").to_ascii_uppercase();
    let method = Method::from_bytes(method_str.as_bytes())?;
    let mut url = http_url(&msg["url"])?;
    let timeout = options["timeout"]
        .as_u64()
        .or_else(|| {
            options["timeout"]
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(|n| n.min(60_000.0).ceil() as u64)
        })
        .filter(|n| *n > 0)
        .unwrap_or(60_000);
    let mut req_headers = Vec::new();
    let mut content_type = None;
    if let Some(headers) = options["headers"].as_object() {
        for (name, value) in headers {
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => bail!("expected string or primitive header value"),
            };
            if name.eq_ignore_ascii_case("content-type") {
                content_type = Some(value.clone());
            }
            req_headers.push((name.as_str(), value));
        }
    }
    let non_empty = |v: &&Value| !v.is_null() && v.as_str() != Some("");
    let body = options.get("body").filter(non_empty);
    let data = body.or_else(|| options.get("form").filter(non_empty));
    let json_body = body.is_some() && is_json_content_type(content_type.as_deref());
    let get_str_query = if method == Method::GET && !json_body {
        data.and_then(Value::as_str)
    } else {
        None
    };
    if let Some(query) = get_str_query {
        let query = query.strip_prefix('?').unwrap_or(query);
        if !query.is_empty() {
            let combined = match url.query() {
                Some(existing) if !existing.is_empty() => format!("{existing}&{query}"),
                _ => query.to_owned(),
            };
            url.set_query(Some(&combined));
        }
    }
    let mut req =
        client.request(method.clone(), url).timeout(Duration::from_millis(timeout.min(60_000)));
    for (name, value) in req_headers {
        req = req.header(name, value);
    }
    if get_str_query.is_none() {
        if let Some(data) = data {
            if method == Method::GET && !json_body {
                req = req.query(data);
            } else if let Some(text) = data.as_str() {
                if content_type.is_none() {
                    req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
                }
                req = req.body(text.to_owned());
            } else if json_body {
                req = req.json(data);
            } else {
                req = req.form(data);
            }
        } else if options.get("formData").is_some_and(|v| !v.is_null()) {
            bail!("unsupported lx.request formData");
        }
    }
    let mut resp = req.send().await.map_err(transport_error)?;
    let status = resp.status();
    let mut headers = Map::new();
    for name in resp.headers().keys() {
        let values: Vec<_> = resp
            .headers()
            .get_all(name)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()))
            .collect();
        headers.insert(
            name.to_string(),
            if name == header::SET_COOKIE {
                json!(values)
            } else {
                json!(values.join(", "))
            },
        );
    }
    let mut raw = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport_error)? {
        ensure!(chunk.len() <= BODY_LIMIT - raw.len(), "HTTP response exceeds 512 KiB");
        raw.extend_from_slice(&chunk);
    }
    Ok(
        json!({"statusCode":status.as_u16(), "statusMessage":status.canonical_reason().unwrap_or(""),
              "headers":headers, "bytes":raw.len(), "raw":STANDARD.encode(raw)}),
    )
}
