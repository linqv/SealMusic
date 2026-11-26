use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::{Client, Response, StatusCode, Url};

use super::{PreparedAudio, Result, Script, api_client, sanitize_log, stream::download_and_decode};
use crate::slint_types::OnlineIssue;

const LIMIT: u64 = 256 * 1024 * 1024;

fn url_has_expired_param(url: &Url) -> bool {
    url.query_pairs().any(|(k, v)| {
        k.eq_ignore_ascii_case("expires")
            && v.parse::<u64>().is_ok_and(|timestamp| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .is_ok_and(|now| timestamp <= now.as_secs())
            })
    })
}

fn expired(resolved_url: &str, response: &Response) -> bool {
    if !matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED | StatusCode::GONE
    ) {
        return false;
    }
    response
        .headers()
        .get("x-amz-error-code")
        .is_some_and(|v| v == "ExpiredToken" || v == "RequestExpired")
        || url_has_expired_param(response.url())
        || Url::parse(resolved_url).is_ok_and(|u| url_has_expired_param(&u))
}

fn origin_label(url: &str) -> String {
    Url::parse(url).map(|u| u.origin().ascii_serialization()).unwrap_or_else(|_| "<invalid>".into())
}

trait UrlResolver {
    fn resolve(&mut self) -> impl std::future::Future<Output = Result<String>> + Send;
}

struct SessionResolver<'a> {
    session: lx_probe::Session,
    action: &'a serde_json::Value,
    context: &'a str,
}

impl UrlResolver for SessionResolver<'_> {
    async fn resolve(&mut self) -> Result<String> {
        self.session.resolve(self.action).await.map_err(|e| {
            log::warn!(
                "LX source resolve failed ({}): {}",
                self.context,
                sanitize_log(&e.to_string())
            );
            (OnlineIssue::Resolve, self.context.to_owned())
        })
    }
}

async fn fetch_response(
    client: &Client,
    context: &str,
    mut resolver: impl UrlResolver,
) -> Result<Response> {
    let transfer = || (OnlineIssue::Transfer, context.to_owned());
    let mut ready = None;
    for attempt in 0..2 {
        let url = resolver.resolve().await?;
        let origin = origin_label(&url);
        let response = client.get(&url).send().await.map_err(|e| {
            log::warn!("Media request failed ({context}, {origin}): {}", e.without_url());
            transfer()
        })?;
        if attempt == 0 && expired(&url, &response) {
            log::info!("Media URL expired ({context}, {origin}); re-resolving once");
            continue;
        }
        if !response.status().is_success() {
            let code = response.status().as_u16();
            log::warn!("Media HTTP error ({context}, {origin}): HTTP {code}");
            return Err((OnlineIssue::Transfer, format!("HTTP {code}")));
        }
        ready = Some(response);
        break;
    }
    ready.ok_or_else(transfer)
}

pub async fn prepare(
    script: Script,
    action: serde_json::Value,
    alert: lx_probe::AlertHandler,
    resume: bool,
    finished: impl FnOnce() + Send + 'static,
) -> Result<PreparedAudio> {
    let source = action["source"].as_str().unwrap_or_default();
    let context = action["info"]["type"]
        .as_str()
        .map(|q| format!("{source} · {q}"))
        .unwrap_or_else(|| source.to_owned());
    let client = api_client().ok_or_else(|| (OnlineIssue::Transfer, context.clone()))?;

    // Drop the script worker subprocess as soon as we have a non-expired media response.
    let response = {
        let session = script.session(alert).await?;
        fetch_response(
            client,
            &context,
            SessionResolver {
                session,
                action: &action,
                context: &context,
            },
        )
        .await?
    };

    download_and_decode(response, &context, LIMIT, resume, finished).await
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader as StdBufReader, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use super::*;

    fn serve(responses: Vec<Vec<u8>>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for raw in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = StdBufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    line.clear();
                }
                let _ = stream.write_all(&raw);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n").into_bytes();
        for (k, v) in headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(b"Connection: close\r\n\r\n");
        out.extend_from_slice(body);
        out
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
    }

    fn client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn flac_bytes() -> Vec<u8> {
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tagged.flac");
        std::fs::read(fixture).unwrap()
    }

    struct FnResolver<F>(F);

    impl<F: FnMut() -> Result<String> + Send> UrlResolver for FnResolver<F> {
        async fn resolve(&mut self) -> Result<String> {
            (self.0)()
        }
    }

    #[test]
    fn downloads_decodes_and_retries_once_on_expired_url() {
        let audio = flac_bytes();
        let len = audio.len().to_string();
        let (origin, server) = serve(vec![
            http_response("403 Forbidden", &[("Content-Length", "0")], b""),
            http_response(
                "200 OK",
                &[("Content-Type", "audio/flac"), ("Content-Length", &len)],
                &audio,
            ),
        ]);
        let calls = Arc::new(AtomicUsize::new(0));
        let decoder = runtime().block_on(async {
            let calls_ref = calls.clone();
            let response = fetch_response(
                &client(),
                "wy · flac",
                FnResolver(move || {
                    let n = calls_ref.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        Ok(format!("{origin}/song.flac?expires=1"))
                    } else {
                        Ok(format!("{origin}/song.flac?expires=4102444800"))
                    }
                }),
            )
            .await
            .unwrap();
            let (done, finished) = futures_channel::oneshot::channel();
            let prepared = download_and_decode(response, "wy · flac", LIMIT, false, move || {
                let _ = done.send(());
            })
            .await
            .unwrap();
            finished.await.unwrap();
            prepared
        });
        server.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(decoder.source.total_duration().is_some_and(|d| !d.is_zero()));
        let embedded = decoder.stream.as_ref().unwrap().take_details().unwrap();
        assert_eq!(embedded.cover.unwrap().dimensions(), (2, 3));
        assert_eq!(embedded.lyrics[0].text, "Embedded line");
    }

    // Keep the HTTP response open until the decoder has actually produced audio.
    // A whole-file implementation cannot pass the gate.
    #[test]
    fn chunked_audio_plays_before_eof_and_detects_a_truncated_transfer() {
        for truncated in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (release, gate) = std::sync::mpsc::channel();
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = StdBufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                    line.clear();
                }
                // 4s of 8kHz mono PCM. Send half of it with no Content-Length or Range.
                let sample_bytes = 64_000u32;
                let mut wav = b"RIFF".to_vec();
                wav.extend((sample_bytes + 36).to_le_bytes());
                wav.extend(b"WAVEfmt ");
                wav.extend(16u32.to_le_bytes());
                wav.extend([1, 0, 1, 0]);
                wav.extend(8000u32.to_le_bytes());
                wav.extend(16000u32.to_le_bytes());
                wav.extend([2, 0, 16, 0]);
                wav.extend(b"data");
                wav.extend(sample_bytes.to_le_bytes());
                wav.resize(44 + sample_bytes as usize / 2, 1);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                    wav.len()
                )
                .unwrap();
                socket.write_all(&wav).unwrap();
                socket.write_all(b"\r\n").unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                if !truncated {
                    let tail = vec![1; sample_bytes as usize / 2];
                    write!(socket, "{:x}\r\n", tail.len()).unwrap();
                    socket.write_all(&tail).unwrap();
                    socket.write_all(b"\r\n0\r\n\r\n").unwrap();
                }
            });
            runtime().block_on(async {
                let response = client().get(url).send().await.unwrap();
                let (done, finished) = futures_channel::oneshot::channel();
                let mut ready = download_and_decode(response, "kw", LIMIT, false, move || {
                    let _ = done.send(());
                })
                .await
                .unwrap();
                assert!(ready.source.next().is_some());
                let stream = ready.stream.as_ref().unwrap();
                assert!(!stream.status().unwrap().0);
                release.send(()).unwrap();
                finished.await.unwrap();
                if truncated {
                    assert_eq!(stream.status().unwrap_err().0, OnlineIssue::Transfer);
                } else {
                    assert!(stream.status().unwrap().0);
                    ready.source.try_seek(Duration::from_secs(1)).unwrap();
                    assert!(ready.source.next().is_some());
                }
            });
            server.join().unwrap();
        }
    }

    #[test]
    fn expired_header_retries_at_most_once_and_non_expired_403_never_retries() {
        // Two consecutive ExpiredToken 403s stop after 2 attempts.
        let (origin, server) = serve(vec![
            http_response(
                "403 Forbidden",
                &[("x-amz-error-code", "ExpiredToken"), ("Content-Length", "0")],
                b"",
            ),
            http_response(
                "403 Forbidden",
                &[("x-amz-error-code", "ExpiredToken"), ("Content-Length", "0")],
                b"",
            ),
        ]);
        let calls = Arc::new(AtomicUsize::new(0));
        let err = runtime().block_on(async {
            let calls_ref = calls.clone();
            let url = format!("{origin}/expired");
            fetch_response(
                &client(),
                "wy · 128k",
                FnResolver(move || {
                    calls_ref.fetch_add(1, Ordering::SeqCst);
                    Ok(url.clone())
                }),
            )
            .await
            .unwrap_err()
        });
        server.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(err, (OnlineIssue::Transfer, "HTTP 403".into()));

        // Plain 403 without expiration proof fails immediately on attempt 1.
        let (origin2, server2) =
            serve(vec![http_response("403 Forbidden", &[("Content-Length", "0")], b"")]);
        let calls2 = Arc::new(AtomicUsize::new(0));
        let err2 = runtime().block_on(async {
            let calls_ref = calls2.clone();
            let url = format!("{origin2}/denied");
            fetch_response(
                &client(),
                "wy · 128k",
                FnResolver(move || {
                    calls_ref.fetch_add(1, Ordering::SeqCst);
                    Ok(url.clone())
                }),
            )
            .await
            .unwrap_err()
        });
        server2.join().unwrap();
        assert_eq!(calls2.load(Ordering::SeqCst), 1);
        assert_eq!(err2, (OnlineIssue::Transfer, "HTTP 403".into()));
    }

    #[test]
    fn enforces_content_length_and_streaming_limits_and_reports_decode_and_resolve_errors() {
        // Resolve failure propagates immediately.
        let resolve_err = runtime().block_on(async {
            fetch_response(
                &client(),
                "tx · 320k",
                FnResolver(|| Err((OnlineIssue::Resolve, "tx · 320k".into()))),
            )
            .await
            .unwrap_err()
        });
        assert_eq!(resolve_err, (OnlineIssue::Resolve, "tx · 320k".into()));

        // Content-Length over limit, streaming body over limit without Content-Length, and bad audio.
        let (origin, server) = serve(vec![
            http_response("200 OK", &[("Content-Length", "1024")], b""),
            http_response("200 OK", &[], &[0_u8; 256]),
            http_response("200 OK", &[("Content-Length", "16")], b"<html>err</html>"),
        ]);
        runtime().block_on(async {
            let c = client();

            let resp_cl =
                fetch_response(&c, "wy · 128k", FnResolver(|| Ok(format!("{origin}/cl"))))
                    .await
                    .unwrap();
            let err_cl =
                download_and_decode(resp_cl, "wy · 128k", 128, false, || {}).await.err().unwrap();
            assert_eq!(err_cl, (OnlineIssue::Transfer, "256 MiB".into()));

            let resp_stream =
                fetch_response(&c, "wy · 128k", FnResolver(|| Ok(format!("{origin}/stream"))))
                    .await
                    .unwrap();
            let err_stream = download_and_decode(resp_stream, "wy · 128k", 128, false, || {})
                .await
                .err()
                .unwrap();
            assert_eq!(err_stream, (OnlineIssue::Transfer, "256 MiB".into()));

            let resp_bad =
                fetch_response(&c, "wy · 128k", FnResolver(|| Ok(format!("{origin}/bad"))))
                    .await
                    .unwrap();
            let err_bad = download_and_decode(resp_bad, "wy · 128k", LIMIT, false, || {})
                .await
                .err()
                .unwrap();
            assert_eq!(err_bad, (OnlineIssue::Decode, "wy · 128k".into()));
        });
        server.join().unwrap();
    }
}
