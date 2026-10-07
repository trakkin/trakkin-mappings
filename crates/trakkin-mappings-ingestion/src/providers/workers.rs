use anyhow::Result;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub(super) fn fetch_many(
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

pub(super) fn parallel_map<Input: Sync, Output: Send>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
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
