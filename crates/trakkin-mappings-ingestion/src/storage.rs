use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub hash: String,
    pub deleted: bool,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct Batch {
    pub records: BTreeMap<String, Value>,
    pub deleted: BTreeMap<String, Value>,
    pub metadata: BTreeMap<String, Value>,
}

pub trait Storage {
    fn index(&mut self) -> Result<BTreeMap<String, Entry>>;
    fn checkpoint(&mut self, operation: &str) -> Result<Value>;
    fn commit(&mut self, operation: &str, checkpoint: &Value, batch: &Batch) -> Result<u64>;
    fn validate(&mut self) -> Result<()>;
    fn maintain(&mut self) -> Result<()>;
}

pub struct RoutedStorage<S> {
    coordinator: S,
    datasets: BTreeMap<String, S>,
}

impl<S: Storage> RoutedStorage<S> {
    pub fn new(coordinator: S, datasets: BTreeMap<String, S>) -> Result<Self> {
        ensure!(!datasets.is_empty(), "Acquisition plan has no datasets");
        Ok(Self {
            coordinator,
            datasets,
        })
    }

    pub fn datasets_mut(&mut self) -> &mut BTreeMap<String, S> {
        &mut self.datasets
    }
}

impl<S: Storage> Storage for RoutedStorage<S> {
    fn index(&mut self) -> Result<BTreeMap<String, Entry>> {
        let mut index = BTreeMap::new();
        for (domain, storage) in &mut self.datasets {
            for (key, entry) in storage.index()? {
                index.insert(format!("{domain}:{key}"), entry);
            }
        }
        Ok(index)
    }

    fn checkpoint(&mut self, operation: &str) -> Result<Value> {
        self.coordinator.checkpoint(operation)
    }

    fn commit(&mut self, operation: &str, checkpoint: &Value, batch: &Batch) -> Result<u64> {
        let mut routed: BTreeMap<String, Batch> = BTreeMap::new();
        for (kind, records) in [
            (&"records", &batch.records),
            (&"deleted", &batch.deleted),
            (&"metadata", &batch.metadata),
        ] {
            for (key, value) in records {
                let (domain, native) =
                    key.split_once(':').context("Unqualified acquisition key")?;
                ensure!(
                    self.datasets.contains_key(domain),
                    "Unplanned acquisition domain {domain}"
                );
                ensure!(
                    !native.is_empty() && !native.starts_with('@'),
                    "Invalid native record ID"
                );
                let target = routed.entry(domain.into()).or_default();
                match *kind {
                    "records" => {
                        target.records.insert(native.into(), value.clone());
                    }
                    "deleted" => {
                        target.deleted.insert(native.into(), value.clone());
                    }
                    _ => {
                        target.metadata.insert(native.into(), value.clone());
                    }
                }
            }
        }
        let mut changed = 0;
        for (domain, batch) in routed {
            changed += self
                .datasets
                .get_mut(&domain)
                .unwrap()
                .commit(operation, checkpoint, &batch)?;
        }
        self.coordinator
            .commit(operation, checkpoint, &Batch::default())?;
        Ok(changed)
    }

    fn validate(&mut self) -> Result<()> {
        self.coordinator.validate()?;
        for storage in self.datasets.values_mut() {
            storage.validate()?;
        }
        Ok(())
    }

    fn maintain(&mut self) -> Result<()> {
        self.coordinator.maintain()?;
        for storage in self.datasets.values_mut() {
            storage.maintain()?;
        }
        Ok(())
    }
}

pub struct Paimon {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Paimon {
    pub fn snapshot_id(&mut self) -> Result<Value> {
        Ok(self.request(json!({"action":"snapshot"}))?["snapshot_id"].take())
    }
    pub fn inspect(
        &mut self,
        key: Option<&str>,
        limit: usize,
        include_deleted: bool,
    ) -> Result<Value> {
        ensure!(
            (1..=1000).contains(&limit),
            "Inspection limit must be between 1 and 1000"
        );
        ensure!(
            !key.is_some_and(|key| key.starts_with('@')),
            "Reserved record key"
        );
        Ok(self.request(
            json!({"action":"inspect","key":key,"limit":limit,"include_deleted":include_deleted}),
        )?["records"]
            .take())
    }

    pub fn open(bridge: &Path, warehouse: &str) -> Result<Self> {
        Self::open_mode(bridge, warehouse, true)
    }

    pub fn open_existing(bridge: &Path, warehouse: &str) -> Result<Self> {
        Self::open_mode(bridge, warehouse, false)
    }

    fn open_mode(bridge: &Path, warehouse: &str, create: bool) -> Result<Self> {
        ensure!(
            bridge.join("classes").is_dir(),
            "Build the Java bridge with mvn -f crates/trakkin-mappings-ingestion/paimon/pom.xml package first"
        );
        let classpath =
            std::env::join_paths([bridge.join("classes"), bridge.join("dependency/*")])?;
        let mut child = Command::new("java")
            .args(["-Xmx2g", "-cp"])
            .arg(classpath)
            .arg("dev.trakkin.ingestion.Main")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Start Paimon Java 2.0 bridge")?;
        let input = child.stdin.take().context("Bridge stdin")?;
        let output = BufReader::new(child.stdout.take().context("Bridge stdout")?);
        let mut storage = Self {
            child,
            input,
            output,
        };
        storage
            .request(json!({"version":1,"action":"open","warehouse":warehouse,"create":create}))?;
        Ok(storage)
    }

    fn request(&mut self, request: Value) -> Result<Value> {
        serde_json::to_writer(&mut self.input, &request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let mut line = String::new();
        if self.output.read_line(&mut line)? == 0 {
            bail!(
                "Storage bridge exited without acknowledging the request; rerun to recover from the committed checkpoint"
            );
        }
        let response: Value = serde_json::from_str(&line).context("Decode storage response")?;
        ensure!(
            response["version"] == 1 && response["ok"] == true,
            "Invalid storage response"
        );
        Ok(response)
    }
}

impl Storage for Paimon {
    fn index(&mut self) -> Result<BTreeMap<String, Entry>> {
        Ok(serde_json::from_value(
            self.request(json!({"action":"index"}))?["entries"].take(),
        )?)
    }
    fn checkpoint(&mut self, operation: &str) -> Result<Value> {
        Ok(
            self.request(json!({"action":"checkpoint","operation":operation}))?["checkpoint"]
                .take(),
        )
    }
    fn commit(&mut self, operation: &str, checkpoint: &Value, batch: &Batch) -> Result<u64> {
        fn prepare(records: &BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>> {
            records
                .iter()
                .map(|(key, record)| {
                    let payload = crate::canonical_json(record)?;
                    let hash = crate::payload_hash(&payload);
                    Ok((key.clone(), json!({"payload":payload, "hash":hash})))
                })
                .collect()
        }
        self.request(
            json!({"action":"commit","operation":operation,"checkpoint":checkpoint,
            "records":prepare(&batch.records)?,"deleted":prepare(&batch.deleted)?,"metadata":batch.metadata}),
        )?["changed"]
            .as_u64()
            .context("Missing commit acknowledgement")
    }
    fn validate(&mut self) -> Result<()> {
        self.request(json!({"action":"validate"}))?;
        Ok(())
    }
    fn maintain(&mut self) -> Result<()> {
        self.request(json!({"action":"maintain"}))?;
        Ok(())
    }
}

impl Drop for Paimon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
