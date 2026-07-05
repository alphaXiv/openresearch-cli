//! `orx serve` — loopback HTTP/SSE surface over the local run store.
//!
//! The jobs sibling of `opencode serve`: orx owns external-run truth (SQLite
//! store + run-log files) and this daemon makes it observable. Locally a UI
//! hits it directly; on an agent box the api SSH-tunnels to it and re-streams
//! (the same lens pattern as opencode's port 4779).
//!
//! Routes:
//!   GET /health                       {"ok":true}
//!   GET /runs                         run list (newest first)
//!   GET /runs/{id}                    one run
//!   GET /runs/{id}/logs[?offset=N]    raw log bytes from offset
//!   GET /event                        SSE: run.updated + run.log events
//!
//! Hand-rolled HTTP/1.1 on a tokio TcpListener (the login.rs idiom) — no
//! framework dependency for a single-tenant loopback daemon.

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{anyhow, Result};
use crate::store::{log_path, Store, StoredRun};

pub async fn run(args: crate::ServeArgs) -> Result<()> {
    let port = args.port.unwrap_or(4790);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow!("Could not bind 127.0.0.1:{}: {}", port, e))?;
    eprintln!("orx serve: listening on http://127.0.0.1:{port}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(x) => x,
            Err(err) => {
                eprintln!("orx serve: accept failed: {err}");
                continue;
            }
        };
        tokio::spawn(async move {
            if let Err(err) = handle(stream).await {
                eprintln!("orx serve: request failed: {err}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream) -> Result<()> {
    // Read the head (requests are header-only GETs; 8 KB is plenty).
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    loop {
        let n = stream.read(&mut buf[len..]).await?;
        if n == 0 {
            return Ok(());
        }
        len += n;
        if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") || len == buf.len() {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..len]);
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" {
        return respond(&mut stream, 405, "application/json", b"{\"error\":\"method\"}").await;
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let query: HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    match path {
        "/health" => respond(&mut stream, 200, "application/json", b"{\"ok\":true}").await,
        "/runs" => {
            let runs = Store::open()?.list_runs(200)?;
            let body = serde_json::to_vec(&serde_json::json!({ "runs": runs }))?;
            respond(&mut stream, 200, "application/json", &body).await
        }
        "/event" => serve_events(&mut stream).await,
        _ => {
            if let Some(rest) = path.strip_prefix("/runs/") {
                match rest.split_once('/') {
                    None => {
                        let Some(run) = Store::open()?.get_run(rest)? else {
                            return respond(
                                &mut stream,
                                404,
                                "application/json",
                                b"{\"error\":\"not_found\"}",
                            )
                            .await;
                        };
                        let body = serde_json::to_vec(&serde_json::json!({ "run": run }))?;
                        respond(&mut stream, 200, "application/json", &body).await
                    }
                    Some((id, "logs")) => {
                        let offset: u64 = query
                            .get("offset")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        let bytes = read_log_from(id, offset);
                        respond(&mut stream, 200, "text/plain; charset=utf-8", &bytes).await
                    }
                    _ => {
                        respond(&mut stream, 404, "application/json", b"{\"error\":\"not_found\"}")
                            .await
                    }
                }
            } else {
                respond(&mut stream, 404, "application/json", b"{\"error\":\"not_found\"}").await
            }
        }
    }
}

fn read_log_from(run_id: &str, offset: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log_path(run_id)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if f.seek(SeekFrom::Start(offset)).is_ok() {
        let _ = f.take(4_000_000).read_to_end(&mut out);
    }
    out
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

/// SSE loop: every 500ms diff the store (status changes → `run.updated`) and
/// each live run's log file (appended bytes → `run.log`). A comment ping every
/// ~15s keeps intermediaries from timing the stream out. Ends when the client
/// disconnects (write fails).
async fn serve_events(stream: &mut TcpStream) -> Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        )
        .await?;

    // Baseline: emit current runs once so a fresh subscriber can render
    // without a separate /runs fetch, then only diffs.
    let mut known: HashMap<String, (String, i64)> = HashMap::new();
    let mut log_offsets: HashMap<String, u64> = HashMap::new();
    {
        let runs = Store::open()?.list_runs(200)?;
        for run in &runs {
            known.insert(run.id.clone(), (run.status.clone(), run.updated_at));
            // Start log tails at the current end — history is on /runs/{id}/logs.
            log_offsets.insert(run.id.clone(), log_size(&run.id));
            write_event(stream, "run.updated", &serde_json::json!({ "run": run })).await?;
        }
    }

    let mut ticks: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        ticks += 1;
        if ticks.is_multiple_of(30) {
            stream.write_all(b": keep-alive\n\n").await?;
        }

        let runs = match Store::open().and_then(|s| s.list_runs(200)) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for run in &runs {
            let changed = match known.get(&run.id) {
                None => true,
                Some((status, updated)) => *status != run.status || *updated != run.updated_at,
            };
            if changed {
                known.insert(run.id.clone(), (run.status.clone(), run.updated_at));
                write_event(stream, "run.updated", &serde_json::json!({ "run": run })).await?;
            }
            emit_log_delta(stream, run, &mut log_offsets).await?;
        }
    }
}

fn log_size(run_id: &str) -> u64 {
    std::fs::metadata(log_path(run_id))
        .map(|m| m.len())
        .unwrap_or(0)
}

async fn emit_log_delta(
    stream: &mut TcpStream,
    run: &StoredRun,
    offsets: &mut HashMap<String, u64>,
) -> Result<()> {
    let offset = *offsets.entry(run.id.clone()).or_insert(0);
    let size = log_size(&run.id);
    if size <= offset {
        return Ok(());
    }
    let chunk = read_log_from(&run.id, offset);
    offsets.insert(run.id.clone(), offset + chunk.len() as u64);
    write_event(
        stream,
        "run.log",
        &serde_json::json!({
            "runId": run.id,
            "offset": offset,
            "chunk": String::from_utf8_lossy(&chunk),
        }),
    )
    .await
}

async fn write_event(
    stream: &mut TcpStream,
    event: &str,
    data: &serde_json::Value,
) -> Result<()> {
    // SSE data must be newline-free per line; JSON-encode guarantees that.
    let frame = format!("event: {event}\ndata: {data}\n\n");
    stream.write_all(frame.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
