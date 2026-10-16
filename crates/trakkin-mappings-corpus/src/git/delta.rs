use super::*;
use std::collections::VecDeque;
use tokio::process::ChildStdout;
use tokio::task::JoinHandle;

pub struct Delta {
    child: Child,
    output: BufReader<ChildStdout>,
    diagnostics: JoinHandle<std::io::Result<Vec<u8>>>,
    repository: Repository,
    pending: VecDeque<(String, Option<String>)>,
    complete: bool,
    lock: Option<RepositoryLock>,
}

impl Delta {
    pub(super) fn open(repository: &Repository, base: &str, commit: &str) -> Result<Self> {
        let lock = repository.lock()?;
        let mut child = repository
            .command(
                &[
                    "diff-tree",
                    "-r",
                    "--no-commit-id",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--no-abbrev",
                    base,
                    commit,
                    "--",
                    "mappings",
                ],
                None,
                None,
            )?
            .spawn()?;
        let output = BufReader::new(child.stdout.take().context("Git output unavailable")?);
        let stderr = child.stderr.take().context("Git diagnostics unavailable")?;
        Ok(Self {
            child,
            output,
            diagnostics: tokio::spawn(read_output(stderr)),
            repository: repository.clone(),
            pending: VecDeque::new(),
            complete: false,
            lock: Some(lock),
        })
    }

    pub async fn next_batch(&mut self, limit: usize) -> Result<Vec<FileChange>> {
        ensure!(
            (1..=1000).contains(&limit),
            "Git batch size must be between 1 and 1000"
        );
        tokio::time::timeout(COMMAND_TIMEOUT, self.read_batch(limit))
            .await
            .context("Git delta timed out")?
    }

    async fn read_batch(&mut self, limit: usize) -> Result<Vec<FileChange>> {
        let mut entries = Vec::new();
        while entries.len() < limit {
            if let Some(entry) = self.pending.pop_front() {
                entries.push(entry);
            } else if self.complete {
                break;
            } else if let Some(entry) = self.read_entry().await? {
                entries.push(entry);
            } else {
                let status = self.child.wait().await?;
                let diagnostics = (&mut self.diagnostics).await??;
                ensure!(
                    status.success(),
                    "Git delta failed: {}",
                    String::from_utf8_lossy(&diagnostics)
                );
                self.complete = true;
            }
        }
        let request = entries
            .iter()
            .filter_map(|(_, object)| object.as_ref())
            .map(|object| format!("{object}\n"))
            .collect::<String>();
        let sizes = self
            .repository
            .text(
                &["cat-file", "--batch-check"],
                Some(request.as_bytes()),
                None,
            )
            .await?;
        let mut sizes = sizes.lines();
        let mut total = 0;
        let mut keep = 0;
        for (_, object) in &entries {
            if let Some(object) = object {
                let fields = sizes
                    .next()
                    .context("missing blob header")?
                    .split_whitespace()
                    .collect::<Vec<_>>();
                ensure!(
                    fields.len() == 3 && fields[0] == object && fields[1] == "blob",
                    "invalid mapping blob"
                );
                let size: usize = fields[2].parse()?;
                ensure!(size <= FILE_LIMIT, "mapping file exceeds 2 MiB");
                if total + size + 256 > OUTPUT_LIMIT {
                    break;
                }
                total += size + 256;
            }
            keep += 1;
        }
        for entry in entries.drain(keep..).rev() {
            self.pending.push_front(entry);
        }
        let request = entries
            .iter()
            .filter_map(|(_, object)| object.as_ref())
            .map(|object| format!("{object}\n"))
            .collect::<String>();
        let output = self
            .repository
            .run(&["cat-file", "--batch"], Some(request.as_bytes()), None)
            .await?;
        let mut remaining = output.as_slice();
        let mut changes = Vec::new();
        for (path, object) in entries {
            let text = if let Some(object) = object {
                let boundary = remaining
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .context("missing blob header")?;
                let fields = std::str::from_utf8(&remaining[..boundary])?
                    .split_whitespace()
                    .collect::<Vec<_>>();
                ensure!(
                    fields.len() == 3 && fields[0] == object && fields[1] == "blob",
                    "inconsistent blob header"
                );
                let size: usize = fields[2].parse()?;
                remaining = &remaining[boundary + 1..];
                ensure!(
                    remaining.len() > size && remaining[size] == b'\n',
                    "inconsistent blob size"
                );
                let text = std::str::from_utf8(&remaining[..size])?.to_owned();
                remaining = &remaining[size + 1..];
                Some(text)
            } else {
                None
            };
            changes.push(FileChange { path, text });
        }
        ensure!(remaining.is_empty(), "unexpected blob output");
        if self.complete && self.pending.is_empty() {
            self.lock.take();
        }
        Ok(changes)
    }

    async fn read_entry(&mut self) -> Result<Option<(String, Option<String>)>> {
        let mut header = Vec::new();
        if (&mut self.output)
            .take(257)
            .read_until(0, &mut header)
            .await?
            == 0
        {
            return Ok(None);
        }
        let mut path = Vec::new();
        (&mut self.output)
            .take(4097)
            .read_until(0, &mut path)
            .await?;
        ensure!(
            header.len() <= 256 && path.len() <= 4096,
            "invalid Git delta entry"
        );
        let path = std::str::from_utf8(path.strip_suffix(&[0]).context("invalid Git path")?)?;
        validate_path(path)?;
        let fields = std::str::from_utf8(
            header
                .strip_suffix(&[0])
                .context("invalid Git delta header")?,
        )?
        .split_whitespace()
        .collect::<Vec<_>>();
        ensure!(
            fields.len() == 5
                && matches!(fields[0], ":000000" | ":100644")
                && matches!(fields[1], "000000" | "100644"),
            "mapping entries must be regular non-executable files"
        );
        let object = (fields[1] != "000000").then(|| fields[3].to_owned());
        Ok(Some((path.to_owned(), object)))
    }
}

impl Drop for Delta {
    fn drop(&mut self) {
        self.diagnostics.abort();
    }
}
