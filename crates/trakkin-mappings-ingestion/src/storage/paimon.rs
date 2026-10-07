use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    rc::Rc,
};

use super::{Batch, Index, Storage, Update};
use crate::Operation;

pub struct Paimon {
    bridge: PaimonBridge,
    warehouse: String,
}

#[derive(Clone)]
pub struct PaimonBridge {
    process: Rc<RefCell<Process>>,
}

struct Process {
    child: Child,
    input: ChildStdin,
    output: Responses,
}

#[derive(Serialize)]
struct Request<'a> {
    version: u8,
    warehouse: &'a str,
    #[serde(flatten)]
    action: Action<'a>,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action<'a> {
    Open {
        create: bool,
    },
    Index,
    Checkpoint {
        operation: Operation,
    },
    Commit {
        operation: Operation,
        checkpoint: &'a Value,
        updates: &'a BTreeMap<String, Update>,
    },
    Snapshot,
    Inspect {
        key: Option<&'a str>,
        limit: usize,
        include_deleted: bool,
    },
    Validate,
    Maintain,
}

#[derive(Deserialize)]
struct Response {
    version: u64,
    ok: bool,
    entries: Option<Index>,
    checkpoint: Option<Value>,
    records: Option<Value>,
    snapshot_id: Option<i64>,
}

type Responses = serde_json::StreamDeserializer<
    'static,
    serde_json::de::IoRead<BufReader<ChildStdout>>,
    Response,
>;

fn validate_response(response: Response) -> Result<Response> {
    ensure!(
        response.version == 1 && response.ok,
        "Invalid storage response"
    );
    Ok(response)
}

impl Response {
    fn index(self) -> Result<Index> {
        self.entries.context("Missing storage index")
    }
}

impl PaimonBridge {
    pub fn start(bridge: &Path) -> Result<Self> {
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
            .context("Start Paimon Java bridge")?;
        let input = child.stdin.take().context("Bridge stdin")?;
        let output = serde_json::Deserializer::from_reader(BufReader::new(
            child.stdout.take().context("Bridge stdout")?,
        ))
        .into_iter();
        Ok(Self {
            process: Rc::new(RefCell::new(Process {
                child,
                input,
                output,
            })),
        })
    }

    pub fn open(&self, warehouse: &str) -> Result<Paimon> {
        self.open_mode(warehouse, true)
    }

    pub fn open_existing(&self, warehouse: &str) -> Result<Paimon> {
        self.open_mode(warehouse, false)
    }

    fn open_mode(&self, warehouse: &str, create: bool) -> Result<Paimon> {
        self.process
            .borrow_mut()
            .request(warehouse, Action::Open { create })?;
        Ok(Paimon {
            bridge: self.clone(),
            warehouse: warehouse.into(),
        })
    }
}

impl Process {
    fn request(&mut self, warehouse: &str, action: Action<'_>) -> Result<Response> {
        let request = Request {
            version: 1,
            warehouse,
            action,
        };
        serde_json::to_writer(&mut self.input, &request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let Some(response) = self.output.next() else {
            bail!(
                "Storage bridge exited without acknowledging the request; rerun to recover from the committed checkpoint"
            );
        };
        validate_response(response.context("Decode storage response")?)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Paimon {
    fn request(&self, action: Action<'_>) -> Result<Response> {
        self.bridge
            .process
            .borrow_mut()
            .request(&self.warehouse, action)
    }

    pub fn snapshot_id(&self) -> Result<Option<i64>> {
        Ok(self.request(Action::Snapshot)?.snapshot_id)
    }

    pub fn inspect(&self, key: Option<&str>, limit: usize, include_deleted: bool) -> Result<Value> {
        ensure!(
            (1..=1000).contains(&limit),
            "Inspection limit must be between 1 and 1000"
        );
        ensure!(
            !key.is_some_and(|key| key.starts_with('@')),
            "Reserved record key"
        );
        let response = self.request(Action::Inspect {
            key,
            limit,
            include_deleted,
        })?;
        response.records.context("Missing inspection records")
    }

    pub fn validate(&self) -> Result<()> {
        self.request(Action::Validate)?;
        Ok(())
    }

    pub fn maintain(&mut self) -> Result<()> {
        self.request(Action::Maintain)?;
        Ok(())
    }
}

impl Storage for Paimon {
    fn index(&mut self) -> Result<Index> {
        self.request(Action::Index)?.index()
    }

    fn checkpoint(&mut self, operation: Operation) -> Result<Value> {
        let response = self.request(Action::Checkpoint { operation })?;
        response.checkpoint.context("Missing storage checkpoint")
    }

    fn commit(&mut self, operation: Operation, checkpoint: &Value, batch: &Batch) -> Result<()> {
        batch.validate()?;
        self.request(Action::Commit {
            operation,
            checkpoint,
            updates: &batch.updates,
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Entry;
    use serde_json::json;

    fn decode(line: &str) -> Result<Response> {
        validate_response(serde_json::from_str(line)?)
    }

    #[test]
    fn response_stream_preserves_consecutive_acknowledgements() {
        let replies = concat!(
            r#"{"version":1,"ok":true,"entries":{}}"#,
            "\n",
            r#"{"version":1,"ok":true,"checkpoint":{"watermark":100}}"#,
            "\n",
        );
        let mut responses =
            serde_json::Deserializer::from_reader(replies.as_bytes()).into_iter::<Response>();
        assert!(
            responses
                .next()
                .unwrap()
                .unwrap()
                .index()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            responses.next().unwrap().unwrap().checkpoint.unwrap()["watermark"],
            100
        );
        assert!(responses.next().is_none());
    }

    #[test]
    fn response_stream_does_not_wait_for_eof_or_another_reply() {
        struct LiveInput(&'static [u8]);

        impl std::io::Read for LiveInput {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if output.is_empty() {
                    return Ok(0);
                }
                assert!(
                    !self.0.is_empty(),
                    "A complete response must not need another read"
                );
                let count = output.len().min(self.0.len());
                output[..count].copy_from_slice(&self.0[..count]);
                self.0 = &self.0[count..];
                Ok(count)
            }
        }

        let input = LiveInput(br#"{"version":1,"ok":true,"entries":{}}"#);
        let mut responses =
            serde_json::Deserializer::from_reader(BufReader::new(input)).into_iter::<Response>();
        assert!(
            responses
                .next()
                .unwrap()
                .unwrap()
                .index()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[ignore = "Requires packaged Java 21 / Paimon 2.0 bridge; run with --ignored"]
    fn warehouse_handles_share_and_own_the_bridge_process() {
        let path = std::env::var_os("TRAKKIN_MAPPINGS_INGESTION_BRIDGE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("paimon/target"));
        let bridge = PaimonBridge::start(&path).unwrap();
        let process = Rc::downgrade(&bridge.process);
        let directory = tempfile::tempdir().unwrap();
        let first = bridge
            .open(directory.path().join("first").to_str().unwrap())
            .unwrap();
        let second = bridge
            .open(directory.path().join("second").to_str().unwrap())
            .unwrap();
        assert!(Rc::ptr_eq(&first.bridge.process, &second.bridge.process));
        let pid = bridge.process.borrow().child.id();
        drop(bridge);
        assert!(first.snapshot_id().unwrap().is_none());
        drop(first);
        assert_eq!(second.bridge.process.borrow().child.id(), pid);
        assert!(second.snapshot_id().unwrap().is_none());
        drop(second);
        assert!(process.upgrade().is_none());
    }

    #[test]
    fn prepared_commits_send_only_updates_and_checkpoint() {
        let batch = Batch {
            updates: ["1", "new"]
                .into_iter()
                .map(|key| {
                    (
                        key.into(),
                        Update::record(&json!({"id":key}), json!({})).unwrap(),
                    )
                })
                .collect(),
        };
        for batch in [&batch, &Batch::default()] {
            let request = serde_json::to_value(Request {
                version: 1,
                warehouse: "fixture",
                action: Action::Commit {
                    operation: Operation::Sync,
                    checkpoint: &json!({}),
                    updates: &batch.updates,
                },
            })
            .unwrap();
            assert_eq!(request["version"], 1);
            assert_eq!(request["warehouse"], "fixture");
            assert_eq!(request["action"], "commit");
            assert_eq!(request["operation"], "sync");
            assert_eq!(request["checkpoint"], json!({}));
            assert_eq!(
                request["updates"].as_object().unwrap().len(),
                batch.updates.len()
            );
            assert!(request.get("entries").is_none());
        }
    }

    #[test]
    fn index_preserves_hashes_tombstones_and_metadata() {
        let entries = Index::from([
            (
                "1".into(),
                Entry {
                    hash: "live".into(),
                    deleted: false,
                    metadata: json!({"parent":"tv:1","address":"episode:1/1/1"}),
                },
            ),
            (
                "2".into(),
                Entry {
                    hash: "deleted".into(),
                    deleted: true,
                    metadata: json!({"materialized_at":100}),
                },
            ),
        ]);
        let line = json!({"version":1,"ok":true,"entries":entries}).to_string();
        assert_eq!(decode(&line).unwrap().index().unwrap(), entries);
    }

    #[test]
    fn rejects_invalid_protocol_and_missing_response_fields() {
        for response in [
            json!({"version":2,"ok":true,"entries":{}}),
            json!({"version":1,"ok":false,"entries":{}}),
            json!({"version":1,"entries":{}}),
            json!({"version":1,"ok":true}),
            json!({"version":1,"ok":true,"entries":{"1":{"deleted":false}}}),
        ] {
            assert!(
                decode(&response.to_string())
                    .and_then(Response::index)
                    .is_err()
            );
        }
    }
}
