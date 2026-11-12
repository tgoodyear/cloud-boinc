//! Fetching the project's dashboard feed (`<dashboard_feed_url>`): one GET
//! with a short timeout, no redirects and a cap on the size read. What comes
//! back is validated by `cb_core::feed::parse_feed`; this only moves bytes.

use std::time::Duration;

use cb_core::feed::{valid_https_url, MAX_FEED_BYTES};
use futures_util::future::{select, Either};
use futures_util::StreamExt;
use worker::{
    AbortController, Delay, Fetch, Headers, Method, Request, RequestInit, RequestRedirect,
};

/// How long the whole fetch, body included, may take.
pub const FEED_TIMEOUT_MS: u64 = 3000;

async fn read(url: &str, signal: &worker::AbortSignal) -> Result<String, String> {
    let headers = Headers::new();
    headers
        .set("Accept", "application/json")
        .map_err(|e| e.to_string())?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get)
        .with_headers(headers)
        // A redirect could lead anywhere, plain `http` included.
        .with_redirect(RequestRedirect::Manual);
    let request = Request::new_with_init(url, &init).map_err(|e| e.to_string())?;
    let mut response = Fetch::Request(request)
        .send_with_signal(signal)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status_code();
    if status != 200 {
        return Err(format!("status {status}"));
    }
    let declared = response
        .headers()
        .get("Content-Length")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_FEED_BYTES as u64) {
        return Err("the document is too large".into());
    }
    let mut stream = response.stream().map_err(|e| e.to_string())?;
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if out.len() + chunk.len() > MAX_FEED_BYTES {
            return Err("the document is too large".into());
        }
        out.extend_from_slice(&chunk);
    }
    String::from_utf8(out).map_err(|_| "the document is not UTF-8".to_string())
}

/// The feed's text, or why it could not be had. The error never contains the
/// URL.
pub async fn fetch_feed(url: &str) -> Result<String, String> {
    if !valid_https_url(url) {
        return Err("the feed URL is not an https address".into());
    }
    let controller = AbortController::default();
    let signal = controller.signal();
    let fetch = read(url, &signal);
    let timeout = Delay::from(Duration::from_millis(FEED_TIMEOUT_MS));
    futures_util::pin_mut!(fetch);
    futures_util::pin_mut!(timeout);
    match select(fetch, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => {
            controller.abort();
            Err("timed out".into())
        }
    }
}
