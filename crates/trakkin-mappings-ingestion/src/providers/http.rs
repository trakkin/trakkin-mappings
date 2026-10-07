use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone)]
pub(super) struct Http {
    pub client: Client,
    interval: Duration,
    next: Arc<Mutex<Instant>>,
}

impl Http {
    pub fn new(interval: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(90))
                .connect_timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("trakkin-mappings-ingestion/1")
                .build()?,
            interval,
            next: Arc::new(Mutex::new(Instant::now())),
        })
    }

    pub fn send(&self, request: RequestBuilder) -> Result<Response> {
        self.send_with_status(request, &[])
    }

    pub fn send_with_status(&self, request: RequestBuilder, accepted: &[u16]) -> Result<Response> {
        let mut delay = Duration::ZERO;
        for attempt in 0..6 {
            std::thread::sleep(delay);
            loop {
                let mut next = self
                    .next
                    .lock()
                    .map_err(|_| anyhow::anyhow!("HTTP limiter poisoned"))?;
                let now = Instant::now();
                if *next <= now {
                    *next = now + self.interval;
                    break;
                }
                let wait = next.saturating_duration_since(now);
                drop(next);
                std::thread::sleep(wait);
            }
            let result = request.try_clone().context("Clone API request")?.send();
            delay = Duration::from_secs(1 << attempt);
            match result {
                Ok(response)
                    if response.status().as_u16() == 429 || response.status().is_server_error() =>
                {
                    let rate_limited = response.status().as_u16() == 429;
                    if let Some(header) = response
                        .headers()
                        .get("retry-after")
                        .and_then(|header| header.to_str().ok())
                    {
                        delay = header
                            .parse::<u64>()
                            .map(Duration::from_secs)
                            .ok()
                            .or_else(|| {
                                httpdate::parse_http_date(header).ok().map(|date| {
                                    date.duration_since(SystemTime::now()).unwrap_or_default()
                                })
                            })
                            .unwrap_or(delay);
                    }
                    if rate_limited {
                        let mut next = self
                            .next
                            .lock()
                            .map_err(|_| anyhow::anyhow!("HTTP limiter poisoned"))?;
                        *next = (*next).max(Instant::now() + delay);
                        drop(next);
                        eprintln!(
                            "Warning: rate limit (HTTP 429) from {}; applying {:.3}s shared cooldown (attempt {}/6)",
                            response.url().host_str().unwrap_or("upstream"),
                            delay.as_secs_f64(),
                            attempt + 1
                        );
                    }
                }
                Ok(response)
                    if response.status().is_success()
                        || response.status().as_u16() == 404
                        || accepted.contains(&response.status().as_u16()) =>
                {
                    return Ok(response);
                }
                Ok(response) => bail!(
                    "Upstream HTTP {} (response body suppressed)",
                    response.status()
                ),
                Err(_) if attempt < 5 => {}
                Err(_) => bail!("Upstream transport failed after retries"),
            }
        }
        bail!("Upstream retry budget exhausted")
    }

    pub fn json(&self, request: RequestBuilder) -> Result<Value> {
        let response = self.send(request)?;
        if !response.status().is_success() {
            bail!("Discovery endpoint not found");
        }
        response.json().context("Decode upstream JSON")
    }
}
