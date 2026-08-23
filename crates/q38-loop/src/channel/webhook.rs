//! Generic HTTP inbound. Same native JSON as QwenPaw `content_parts`.
//!
//! POST /inbound  (also POST /)
//! GET  /health

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::{Error, Result};

use super::envelope::NativePayload;
use super::manager::ChannelManager;
use super::ChannelEndpoint;

pub fn bind_addr(ep: &ChannelEndpoint) -> String {
    if !ep.bind.trim().is_empty() {
        return ep.bind.clone();
    }
    ep.extra
        .get("bind")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1:8788".into())
}

pub async fn serve(ep: ChannelEndpoint, mgr: ChannelManager) -> Result<()> {
    let addr = bind_addr(&ep);
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("q38 channel webhook {} on http://{addr}", ep.id);
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mgr = mgr.clone();
        let ep = ep.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(&mut sock, &mgr, &ep).await {
                let _ = e;
            }
        });
    }
}

async fn handle_conn(
    sock: &mut tokio::net::TcpStream,
    mgr: &ChannelManager,
    ep: &ChannelEndpoint,
) -> Result<()> {
    sock.set_nodelay(true).ok();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let n = tokio::time::timeout_at(deadline, sock.read(&mut tmp))
            .await
            .map_err(|_| Error::msg("webhook read timeout"))??;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 2 * 1024 * 1024 {
            return write_http(sock, 413, b"{\"ok\":false}").await;
        }
        if let Some(header_end) = find_headers(&buf) {
            let headers = std::str::from_utf8(&buf[..header_end])
                .unwrap_or("")
                .to_string();
            let content_len = content_length(&headers).unwrap_or(0);
            let body_start = header_end + 4;
            while buf.len() < body_start + content_len {
                let n = tokio::time::timeout_at(deadline, sock.read(&mut tmp))
                    .await
                    .map_err(|_| Error::msg("webhook body timeout"))??;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let req_line = headers.lines().next().unwrap_or("");
            let method_path = req_line.split_whitespace().take(2).collect::<Vec<_>>();
            let method = method_path.first().copied().unwrap_or("");
            let path = method_path.get(1).copied().unwrap_or("/");
            if method.eq_ignore_ascii_case("GET") && path.starts_with("/health") {
                return write_http(sock, 200, b"{\"ok\":true}").await;
            }
            if !method.eq_ignore_ascii_case("POST") {
                return write_http(sock, 405, b"{\"ok\":false}").await;
            }
            if let Some(secret) =
                nonempty(&ep.secret).or_else(|| ep.extra.get("secret").map(|s| s.as_str()))
            {
                if !header_has_token(&headers, secret) {
                    return write_http(sock, 403, b"{\"ok\":false,\"error\":\"secret\"}").await;
                }
            }
            let body = &buf[body_start..body_start + content_len.min(buf.len() - body_start)];
            let mut env: NativePayload = serde_json::from_slice(body).map_err(Error::msg)?;
            if env.channel.is_empty() {
                env.channel = if ep.kind.is_empty() {
                    ep.id.clone()
                } else {
                    ep.kind.clone()
                };
            }
            if env.reply_url().is_none() && !ep.reply_url.is_empty() {
                env.meta.insert(
                    "reply_url".into(),
                    serde_json::Value::String(ep.reply_url.clone()),
                );
            }
            match mgr.ingest(env).await {
                Ok(r) if r.denied.is_some() => {
                    let msg = format!("{{\"ok\":false,\"error\":\"{}\"}}", r.denied.unwrap());
                    return write_http(sock, 403, msg.as_bytes()).await;
                }
                Ok(r) => {
                    let msg = format!("{{\"ok\":true,\"session\":\"{}\"}}", r.session_id);
                    return write_http(sock, 202, msg.as_bytes()).await;
                }
                Err(e) => {
                    let msg = format!("{{\"ok\":false,\"error\":\"{e}\"}}");
                    return write_http(sock, 500, msg.as_bytes()).await;
                }
            }
        }
    }
    Ok(())
}

fn find_headers(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let (k, v) = line.split_once(':')?;
        if k.eq_ignore_ascii_case("content-length") {
            return v.trim().parse().ok();
        }
    }
    None
}

fn header_has_token(headers: &str, secret: &str) -> bool {
    for line in headers.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.eq_ignore_ascii_case("x-q38-token") || k.eq_ignore_ascii_case("authorization") {
            let v = v.trim();
            if v == secret || v.strip_prefix("Bearer ").is_some_and(|t| t == secret) {
                return true;
            }
        }
    }
    false
}

async fn write_http(sock: &mut tokio::net::TcpStream, status: u16, body: &[u8]) -> Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    sock.flush().await?;
    Ok(())
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}
