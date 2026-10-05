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
    pacing: Arc<Pacing>,
}

struct Pacing {
    interval: Duration,
    next: Mutex<Instant>,
}

impl Pacing {
    fn wait_for_slot(&self) -> Result<Instant> {
        loop {
            let mut next = self
                .next
                .lock()
                .map_err(|_| anyhow::anyhow!("HTTP limiter poisoned"))?;
            let now = Instant::now();
            if *next <= now {
                *next = now + self.interval;
                return Ok(now);
            }
            let wait = next.saturating_duration_since(now);
            drop(next);
            std::thread::sleep(wait);
        }
    }

    fn defer_until(&self, deadline: Instant) -> Result<()> {
        let mut next = self
            .next
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP limiter poisoned"))?;
        *next = (*next).max(deadline);
        Ok(())
    }
}

impl Http {
    pub fn new(interval: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(90))
                .connect_timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("trakkin-mappings-source/1")
                .build()?,
            pacing: Arc::new(Pacing {
                interval,
                next: Mutex::new(Instant::now()),
            }),
        })
    }

    pub fn send(&self, request: RequestBuilder) -> Result<Response> {
        self.send_with_status(request, &[])
    }

    pub fn send_with_status(&self, request: RequestBuilder, accepted: &[u16]) -> Result<Response> {
        self.send_and_read(request, accepted, Ok)
    }

    fn send_and_read<T>(
        &self,
        request: RequestBuilder,
        accepted: &[u16],
        read: impl Fn(Response) -> reqwest::Result<T>,
    ) -> Result<T> {
        let mut delay = Duration::ZERO;
        for attempt in 0..6 {
            std::thread::sleep(delay);
            self.pacing.wait_for_slot()?;
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
                        self.pacing.defer_until(Instant::now() + delay)?;
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
                    match read(response) {
                        Ok(result) => return Ok(result),
                        Err(_) if attempt < 5 => {}
                        Err(_) => bail!("Upstream response body failed after retries"),
                    }
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
        self.json_optional(request)?
            .context("Discovery endpoint not found")
    }

    pub fn json_optional(&self, request: RequestBuilder) -> Result<Option<Value>> {
        let body = self.send_and_read(request, &[], |response| {
            if response.status().as_u16() == 404 {
                Ok(None)
            } else {
                response.bytes().map(Some)
            }
        })?;
        body.map(|body| serde_json::from_slice(&body).context("Decode upstream JSON"))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn cloned_clients_share_request_spacing_and_cooldowns() {
        let interval = Duration::from_millis(2);
        let http = Http::new(interval).unwrap();
        let clients = [http.clone(), http.clone(), http.clone(), http.clone()];
        let barrier = Barrier::new(clients.len());
        let mut slots = std::thread::scope(|scope| {
            let workers = clients
                .iter()
                .map(|client| {
                    scope.spawn(|| {
                        barrier.wait();
                        client.pacing.wait_for_slot().unwrap()
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>()
        });
        slots.sort();
        assert!(
            slots
                .windows(2)
                .all(|pair| pair[1].duration_since(pair[0]) >= interval)
        );
        let deadline = Instant::now() + interval;
        http.pacing.defer_until(deadline).unwrap();
        assert!(clients[0].pacing.wait_for_slot().unwrap() >= deadline);
    }
}
