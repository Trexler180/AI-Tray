//! The one authenticated GET every live endpoint goes through. `live` and
//! `resets` used to carry a copy each; this is that code, once.

use serde_json::Value;
use std::time::Duration;

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build()
}

/// GET with bearer auth. Returns Ok(json), or Err(status) where 401/403 signal
/// a rejected token and 0 signals a transport or decode error.
pub fn get_json(url: &str, token: &str, extra: &[(&str, &str)]) -> Result<Value, u16> {
    let mut req = agent()
        .get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json");
    // Later `set`s replace earlier ones, so a caller's own Content-Type wins.
    for (k, v) in extra {
        req = req.set(k, v);
    }
    match req.call() {
        Ok(resp) => {
            let body = resp.into_string().map_err(|_| 0u16)?;
            serde_json::from_str(&body).map_err(|_| 0u16)
        }
        Err(ureq::Error::Status(code, _)) => Err(code),
        Err(_) => Err(0),
    }
}

/// Like `get_json`, but retries transport errors and 5xx with backoff
/// (3 attempts total). Auth errors and 429 pass straight through so the caller
/// can refresh the token or back off.
pub fn get_json_retrying(url: &str, token: &str, extra: &[(&str, &str)]) -> Result<Value, u16> {
    let mut delay_ms = 500;
    for _ in 0..2 {
        match get_json(url, token, extra) {
            Err(0) | Err(500) | Err(502) | Err(503) | Err(504) => {
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms *= 3;
            }
            other => return other,
        }
    }
    get_json(url, token, extra)
}
