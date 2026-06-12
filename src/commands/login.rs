//! The `login` command. Loopback OAuth flow.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::browser::open_browser;
use crate::config::{default_api_url, save_credentials, Credentials};
use crate::error::{anyhow, Result};

const SUCCESS_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>OpenResearch CLI</title>\n<body style=\"font-family:system-ui;display:grid;place-items:center;height:100vh;margin:0\">\n<div style=\"text-align:center\">\n<h1>You're logged in</h1><p>You can close this tab and return to your terminal.</p>\n</div></body>";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Loopback OAuth: spin up a throwaway HTTP listener on a random localhost port,
/// send the user through the browser to `{api}/auth/cli/login`, and wait for the
/// API to redirect back here with a freshly minted personal access token.
pub async fn run(args: crate::LoginArgs) -> Result<()> {
    let api_url = args.api_url.unwrap_or_else(default_api_url);
    let nonce = uuid::Uuid::new_v4().to_string();

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let login_url = format!("{api_url}/auth/cli/login?port={port}&state={nonce}");
    println!("Opening your browser to log in\u{2026}");
    println!("If it doesn't open, visit:\n  {login_url}\n");
    open_browser(&login_url);

    let token = tokio::time::timeout(LOGIN_TIMEOUT, accept_callback(&listener, &nonce))
        .await
        .map_err(|_| anyhow!("Login timed out after 5 minutes."))??;

    save_credentials(&Credentials { api_url, token }).await?;
    println!("\u{2713} Logged in. Credentials saved.");

    // Install the `orx` skill shim into any coding agent already set up here, so
    // it auto-discovers how to drive the CLI. Best-effort: never fail login over
    // it.
    crate::commands::install_skills::install_present_quietly().await;

    Ok(())
}

/// Accepts connections until one hits `/callback` with valid token + state.
/// Non-callback paths get a 404 and we keep waiting (matching the TS server
/// which only resolves/rejects on the `/callback` path).
async fn accept_callback(listener: &TcpListener, nonce: &str) -> Result<String> {
    loop {
        let (stream, _) = listener.accept().await?;
        match handle_connection(stream, nonce).await {
            Some(result) => return result,
            None => continue,
        }
    }
}

/// Returns `Some(Ok(token))` / `Some(Err(..))` once the `/callback` path is hit;
/// `None` for non-callback requests so the caller keeps listening.
async fn handle_connection(mut stream: TcpStream, nonce: &str) -> Option<Result<String>> {
    let request_line = match read_request_line(&mut stream).await {
        Ok(line) => line,
        Err(_) => return None,
    };

    // Request line: "METHOD <target> HTTP/1.1"
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if path != "/callback" {
        let _ = write_response(&mut stream, "404 Not Found", "text/plain", "").await;
        return None;
    }

    let mut received: Option<String> = None;
    let mut state: Option<String> = None;
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let decoded = urlencoding::decode(value)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| value.to_string());
        match key {
            "token" => received = Some(decoded),
            "state" => state = Some(decoded),
            _ => {}
        }
    }

    let token = received.filter(|t| !t.is_empty());
    if token.is_none() || state.as_deref() != Some(nonce) {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            "Invalid login response.",
        )
        .await;
        return Some(Err(anyhow!(
            "Login failed: invalid or mismatched response from the server."
        )));
    }

    let _ = write_response(&mut stream, "200 OK", "text/html", SUCCESS_HTML).await;
    Some(Ok(token.unwrap()))
}

/// Reads from the stream until the end of the first request line (CRLF or LF).
async fn read_request_line(stream: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            buf.push(byte[0]);
        }
        if buf.len() > 8192 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
