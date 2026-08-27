//! Shared LLM HTTP client and transient retry for flaky endpoints.
//!
//! Keep the turn alive across blips: longer connect, TCP/H2 keepalive,
//! and automatic retries until the path is back (or the user stops).

use std::future::Future;
use std::time::{Duration, Instant};

use reqwest::Client;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::tool_calls::CancelFlag;

/// Floor for remote TLS/connect. Loopback keeps the configured value.
pub const CONNECT_TIMEOUT_FLOOR_S: u64 = 20;

/// Keep retrying transient failures for this long, then surface the last error.
pub const RETRY_BUDGET: Duration = Duration::from_secs(600);

const BACKOFF_CAP_S: u64 = 20;
const TCP_KEEPALIVE_S: u64 = 30;
const H2_KEEPALIVE_S: u64 = 15;

/// Live/status copy. Console treats this think text as the reconnecting phase.
pub const NET_RETRY_HINT: &str = "网络不稳，正在重连";

pub fn retry_status_line(attempt: u32, wait: Duration) -> String {
    format!(
        "{NET_RETRY_HINT}（第{attempt}次，{}s 后）…\n",
        wait.as_secs().max(1)
    )
}

pub fn effective_connect_timeout_s(cfg: &Config) -> u64 {
    effective_connect_timeout_s_for(cfg.server.connect_timeout_s, &cfg.server.base_url)
}

pub fn effective_connect_timeout_s_for(configured: u64, base_url: &str) -> u64 {
    let n = configured.max(1);
    if is_loopback_base(base_url) {
        n
    } else {
        n.max(CONNECT_TIMEOUT_FLOOR_S)
    }
}

pub fn is_loopback_base(base_url: &str) -> bool {
    let u = base_url.trim().to_ascii_lowercase();
    if u.is_empty() {
        return false;
    }
    u.contains("127.0.0.1") || u.contains("localhost") || u.contains("[::1]")
}

/// Completions / Responses client: keepalive + connect floor for remote hosts.
pub fn stream_client(cfg: &Config) -> Result<Client> {
    build_client(
        effective_connect_timeout_s(cfg),
        cfg.server.read_timeout_s.max(5),
    )
}

/// Short probes (`GET /models`) should not inherit the stream connect floor.
pub fn probe_client(connect_s: u64, timeout_s: u64) -> Result<Client> {
    build_client(connect_s.max(1), timeout_s.max(5))
}

pub fn build_client(connect_s: u64, timeout_s: u64) -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(connect_s.max(1)))
        .timeout(Duration::from_secs(timeout_s.max(5)))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_S))
        .http2_keep_alive_interval(Duration::from_secs(H2_KEEPALIVE_S))
        .http2_keep_alive_timeout(Duration::from_secs(H2_KEEPALIVE_S))
        .http2_keep_alive_while_idle(true)
        .build()
        .map_err(|e| Error::Http(e.to_string()))
}

pub fn is_transient(err: &Error) -> bool {
    match err {
        Error::Watchdog => false,
        Error::Config(_) | Error::Template(_) | Error::Tokenizer(_) | Error::Vendor(_) => false,
        Error::Io(_) => true,
        Error::Http(s) | Error::Msg(s) => looks_transient(s),
    }
}

fn looks_transient(raw: &str) -> bool {
    let l = raw.to_ascii_lowercase();
    if l.contains("认证失败")
        || l.contains("grok login")
        || l.contains("过期")
        || l.contains("expired")
        || l.contains("unauthorized")
        || l.contains("forbidden")
    {
        return false;
    }
    if let Some(code) = http_status_in(&l) {
        return matches!(code, 408 | 409 | 425 | 429 | 500..=599);
    }
    const NEEDLES: &[&str] = &[
        "timed out",
        "timeout",
        "time out",
        "error sending request",
        "error decoding",
        "connection reset",
        "connection abort",
        "connection closed",
        "connection refused",
        "broken pipe",
        "unexpected eof",
        "unexpected end",
        "end of file",
        "incomplete",
        "reset by peer",
        "network",
        "offline",
        "dns error",
        "dns_probe",
        "name or service not known",
        "failed to lookup",
        "temporarily",
        "try again",
        "unavailable",
        "handshake",
        "tls",
        "hyper::error",
        "http2",
        "goaway",
        "stream closed",
        "error reading a body from connection",
        "rate limit",
        "overloaded",
        "temporar",
    ];
    NEEDLES.iter().any(|n| l.contains(n))
}

fn http_status_in(s: &str) -> Option<u16> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && (i == 0 || !bytes[i - 1].is_ascii_digit())
            && (i + 3 == bytes.len() || !bytes[i + 3].is_ascii_digit())
        {
            let n = (bytes[i] - b'0') as u16 * 100
                + (bytes[i + 1] - b'0') as u16 * 10
                + (bytes[i + 2] - b'0') as u16;
            if (400..600).contains(&n) {
                return Some(n);
            }
        }
        i += 1;
    }
    None
}

pub fn retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_secs((1u64 << shift).min(BACKOFF_CAP_S))
}

/// Retry `op` on transient errors until success, cancel, or [`RETRY_BUDGET`].
pub async fn retry_transient<T, F, Fut>(
    cancel: &CancelFlag,
    mut op: F,
    mut on_retry: impl FnMut(u32, Duration, &Error),
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(Error::msg("aborted"));
        }
        match op().await {
            Ok(v) => return Ok(v),
            Err(_) if cancel.is_cancelled() => return Err(Error::msg("aborted")),
            Err(e) if is_transient(&e) => {
                attempt += 1;
                if started.elapsed() >= RETRY_BUDGET {
                    return Err(e);
                }
                let wait = retry_delay(attempt);
                on_retry(attempt, wait, &e);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::msg("aborted")),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_connect_floor_covers_old_five_second_default() {
        assert_eq!(effective_connect_timeout_s_for(5, ""), 20);
        assert_eq!(
            effective_connect_timeout_s_for(5, "https://api.example.com/v1"),
            20
        );
        assert_eq!(
            effective_connect_timeout_s_for(45, "https://api.example.com/v1"),
            45
        );
        assert_eq!(
            effective_connect_timeout_s_for(3, "http://127.0.0.1:8080/v1"),
            3
        );
        assert_eq!(
            effective_connect_timeout_s_for(3, "http://localhost:8080/v1"),
            3
        );
    }

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(retry_delay(1).as_secs(), 1);
        assert_eq!(retry_delay(2).as_secs(), 2);
        assert_eq!(retry_delay(3).as_secs(), 4);
        assert_eq!(retry_delay(6).as_secs(), 20);
        assert_eq!(retry_delay(9).as_secs(), 20);
    }

    #[test]
    fn retries_timeouts_and_5xx_not_auth() {
        assert!(is_transient(&Error::Http(
            "error sending request for url: operation timed out".into()
        )));
        assert!(is_transient(&Error::Http(
            "error decoding response body: connection reset by peer".into()
        )));
        assert!(is_transient(&Error::Http(
            "responses 502 Bad Gateway".into()
        )));
        assert!(is_transient(&Error::Http("429 Too Many Requests".into())));
        assert!(is_transient(&Error::Http(
            "error reading a body from connection".into()
        )));
        assert!(!is_transient(&Error::Http(
            "认证失败。请运行 `grok login`".into()
        )));
        assert!(!is_transient(&Error::msg(
            "认证失败。请运行 `grok login`，或检查 XAI_API_KEY。"
        )));
        assert!(!is_transient(&Error::Http(
            "400 Bad Request: model not found".into()
        )));
        assert!(!is_transient(&Error::Watchdog));
    }

    #[test]
    fn status_line_is_detectable() {
        let line = retry_status_line(2, Duration::from_secs(4));
        assert!(line.contains(NET_RETRY_HINT));
        assert!(line.contains("第2次"));
    }

    #[tokio::test(start_paused = true)]
    async fn retries_transient_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let cancel = CancelFlag::new();
        let n = AtomicU32::new(0);
        let out = retry_transient(
            &cancel,
            || {
                let i = n.fetch_add(1, Ordering::SeqCst);
                async move {
                    if i == 0 {
                        Err(Error::Http("connection reset".into()))
                    } else {
                        Ok(7u8)
                    }
                }
            },
            |_, _, _| {},
        )
        .await
        .unwrap();
        assert_eq!(out, 7);
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }
}
