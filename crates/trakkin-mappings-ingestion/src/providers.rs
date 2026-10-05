use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone)]
pub(crate) struct Http {
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

fn id(record: &Value, field: &str) -> Result<u64> {
    record[field]
        .as_u64()
        .filter(|id| *id > 0)
        .context("Missing positive upstream record ID")
}

fn fetch_many(
    keys: &[String],
    concurrency: usize,
    fetch: impl Fn(&str) -> Result<Option<Value>> + Sync,
) -> Result<BTreeMap<String, Option<Value>>> {
    Ok(
        parallel_map(keys, concurrency, |key| Ok((key.clone(), fetch(key)?)))?
            .into_iter()
            .collect(),
    )
}

fn parallel_map<Input: Sync, Output: Send>(
    inputs: &[Input],
    concurrency: usize,
    fetch: impl Fn(&Input) -> Result<Output> + Sync,
) -> Result<Vec<Output>> {
    anyhow::ensure!(concurrency > 0, "Worker concurrency must be positive");
    let next = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..inputs.len().min(concurrency) {
            let fetch = &fetch;
            let next = &next;
            let cancelled = &cancelled;
            workers.push(scope.spawn(move || -> Result<Vec<(usize, Output)>> {
                let mut records = Vec::new();
                while !cancelled.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(key) = inputs.get(index) else { break };
                    match fetch(key) {
                        Ok(record) => {
                            records.push((index, record));
                        }
                        Err(error) => {
                            cancelled.store(true, Ordering::Relaxed);
                            return Err(error);
                        }
                    }
                }
                Ok(records)
            }));
        }
        let mut records = Vec::new();
        for worker in workers {
            records.extend(
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("Provider fetch worker panicked"))??,
            );
        }
        records.sort_by_key(|(index, _)| *index);
        Ok(records.into_iter().map(|(_, record)| record).collect())
    })
}

fn array<'a>(record: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    record[field]
        .as_array()
        .context("Missing upstream catalogue array")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn workers_overlap_without_exceeding_the_budget_and_preserve_order() {
        let barrier = Barrier::new(3);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let inputs: Vec<_> = (0..12).collect();
        let outputs = parallel_map(&inputs, 3, |input| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(count, Ordering::SeqCst);
            barrier.wait();
            active.fetch_sub(1, Ordering::SeqCst);
            barrier.wait();
            Ok(input * 2)
        })
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(
            outputs,
            inputs.iter().map(|input| input * 2).collect::<Vec<_>>()
        );
        assert!(parallel_map(&inputs, 0, |input| Ok(*input)).is_err());
        assert!(
            parallel_map(&inputs, 3, |_| -> Result<usize> {
                bail!("acquisition failed")
            })
            .is_err()
        );
    }
}
