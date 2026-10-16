use crate::{Artifact, ConflictPolicy, POLICY};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const FILE_LIMIT: usize = 2 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const CACHE_APPLIED: &str = "refs/trakkin/applied";

mod delta;
pub use delta::Delta;

#[derive(Clone, Debug)]
pub struct Repository {
    path: PathBuf,
    credentials: Credentials,
}

#[derive(Clone, Copy, Debug)]
enum Credentials {
    Public,
    Publisher,
}

#[derive(Clone, Debug)]
pub struct History(Repository);

#[derive(Clone, Debug)]
pub struct UpstreamCache(Repository);

impl Deref for History {
    type Target = Repository;
    fn deref(&self) -> &Repository {
        &self.0
    }
}

impl Deref for UpstreamCache {
    type Target = Repository;
    fn deref(&self) -> &Repository {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct RepositoryStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prepared {
    pub base: String,
    pub commit: String,
    pub changes: BTreeMap<String, Option<String>>,
    pub message: String,
    pub date: String,
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    pub text: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("mapping repository revision changed")]
    RevisionChanged,
    #[error(transparent)]
    Operation(#[from] anyhow::Error),
}

struct RepositoryLock(fs::File);
impl Drop for RepositoryLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl RepositoryStore {
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        ensure!(
            fs::symlink_metadata(root)?.is_dir(),
            "repository storage must be a real directory"
        );
        Ok(Self {
            root: root.canonicalize()?,
        })
    }
    pub fn local_path(&self) -> PathBuf {
        self.root.join("local")
    }
    pub async fn local(&self) -> Result<History> {
        History::open(&self.local_path()).await
    }
    pub fn upstream(&self, id: &str) -> Result<UpstreamCache> {
        ensure!(
            id != "local"
                && !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
            "invalid mapping source ID"
        );
        let path = self.root.join(format!("{id}.git"));
        if path.exists() {
            ensure!(
                fs::symlink_metadata(&path)?.is_dir(),
                "mapping cache must be a real directory"
            );
        }
        Ok(UpstreamCache(Repository {
            path,
            credentials: Credentials::Public,
        }))
    }
    pub async fn validate_upstream(&self, remote: &str, branch: &str) -> Result<String> {
        let remote = validate_remote(remote)?;
        validate_branch(branch)?;
        let repository = Repository {
            path: self.root.clone(),
            credentials: Credentials::Public,
        };
        repository
            .run(
                &["check-ref-format", &format!("refs/heads/{branch}")],
                None,
                None,
            )
            .await?;
        Ok(remote)
    }
    pub fn remove_upstream(&self, id: &str) -> Result<()> {
        let repository = self.upstream(id)?;
        let _guard = repository.lock()?;
        if repository.path.exists() {
            ensure!(
                repository.path.join("HEAD").is_file() && repository.path.join("objects").is_dir(),
                "cache is not a bare Git repository"
            );
            fs::remove_dir_all(&repository.path)?;
        }
        Ok(())
    }
}

impl History {
    pub async fn open(path: &Path) -> Result<Self> {
        if path.exists() {
            ensure!(
                fs::symlink_metadata(path)?.is_dir(),
                "repository must be a real directory"
            );
            ensure!(
                path.join("HEAD").is_file() && path.join("objects").is_dir(),
                "existing directory is not a bare Git repository"
            );
        } else {
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            fs::create_dir_all(parent)?;
            let staging = tempfile::Builder::new()
                .prefix(".mapping-init-")
                .tempdir_in(parent)?;
            let repository = Self(Repository {
                path: staging.path().to_owned(),
                credentials: Credentials::Publisher,
            });
            repository
                .run(
                    &["init", "--bare", "--template=", "--initial-branch=main"],
                    None,
                    None,
                )
                .await?;
            let tree = repository.empty_tree().await?;
            let mut command = repository.command(
                &["commit-tree", &tree, "-m", "Initialize mappings"],
                None,
                None,
            )?;
            command
                .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z");
            let commit = String::from_utf8(repository.execute(command, None).await?)?;
            repository
                .run(&["update-ref", "HEAD", commit.trim()], None, None)
                .await?;
            fs::rename(staging.path(), path).context("cannot finish repository initialization")?;
        }
        let repository = Self(Repository {
            path: path.canonicalize()?,
            credentials: Credentials::Publisher,
        });
        repository.safe_config().await?;
        repository.head().await?;
        Ok(repository)
    }
}

impl Repository {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub async fn head(&self) -> Result<String> {
        Ok(self
            .text(&["rev-parse", "--verify", "HEAD^{commit}"], None, None)
            .await?
            .trim()
            .to_owned())
    }
    pub async fn contains_commit(&self, commit: &str) -> Result<bool> {
        validate_commit(commit)?;
        let request = format!("{commit}\n");
        let output = self
            .text(
                &["cat-file", "--batch-check=%(objecttype)"],
                Some(request.as_bytes()),
                None,
            )
            .await?;
        match output.trim() {
            "commit" => Ok(true),
            missing if missing == format!("{commit} missing") => Ok(false),
            _ => anyhow::bail!("revision does not identify a Git commit"),
        }
    }
    async fn empty_tree(&self) -> Result<String> {
        Ok(self
            .text(
                &["hash-object", "-w", "-t", "tree", "--stdin"],
                Some(b""),
                None,
            )
            .await?
            .trim()
            .to_owned())
    }

    pub async fn policy(&self, commit: &str) -> Result<ConflictPolicy> {
        validate_commit(commit)?;
        let listing = self
            .text(&["ls-tree", commit, "--", POLICY], None, None)
            .await?;
        if listing.is_empty() {
            return Ok(ConflictPolicy::default());
        }
        ensure!(
            listing.starts_with("100644 blob "),
            "policy must be a regular file"
        );
        ConflictPolicy::parse(
            &self
                .text(&["show", &format!("{commit}:{POLICY}")], None, None)
                .await?,
        )
    }

    pub async fn delta(&self, base: Option<&str>, commit: &str) -> Result<Delta> {
        validate_commit(commit)?;
        let base = if let Some(base) = base {
            validate_commit(base)?;
            base.to_owned()
        } else {
            self.empty_tree().await?
        };
        Delta::open(self, &base, commit)
    }
}

impl UpstreamCache {
    pub async fn fetch(&self, remote: &str, branch: &str, applied: Option<&str>) -> Result<String> {
        let remote = validate_remote(remote)?;
        self.fetch_validated(&remote, branch, applied).await
    }
    async fn fetch_validated(
        &self,
        remote: &str,
        branch: &str,
        applied: Option<&str>,
    ) -> Result<String> {
        validate_branch(branch)?;
        let _guard = self.lock()?;
        self.run(
            &["check-ref-format", &format!("refs/heads/{branch}")],
            None,
            None,
        )
        .await?;
        if !self.path.exists() {
            let advertised = self
                .text(
                    &[
                        "ls-remote",
                        "--refs",
                        remote,
                        &format!("refs/heads/{branch}"),
                    ],
                    None,
                    None,
                )
                .await?;
            let (commit, reference) = advertised
                .trim()
                .split_once('\t')
                .context("upstream branch does not exist")?;
            validate_commit(commit)?;
            ensure!(
                reference == format!("refs/heads/{branch}"),
                "ambiguous upstream branch"
            );
            let staging = tempfile::Builder::new()
                .prefix(".mapping-fetch-init-")
                .tempdir_in(self.path.parent().context("cache has no parent")?)?;
            let initializing = Repository {
                path: staging.path().to_owned(),
                credentials: Credentials::Public,
            };
            initializing
                .run(
                    &[
                        "init",
                        "--bare",
                        "--template=",
                        if commit.len() == 64 {
                            "--object-format=sha256"
                        } else {
                            "--object-format=sha1"
                        },
                    ],
                    None,
                    None,
                )
                .await?;
            fs::rename(staging.path(), &self.path)
                .context("cannot finish upstream cache initialization")?;
        }
        self.safe_config().await?;
        if let Some(applied) = applied
            && self.contains_commit(applied).await?
        {
            self.run(&["update-ref", CACHE_APPLIED, applied], None, None)
                .await?;
        } else {
            self.run(&["update-ref", "-d", CACHE_APPLIED], None, None)
                .await?;
        }
        self.run(
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-auto-maintenance",
                "--depth=1",
                remote,
                &format!("+refs/heads/{branch}:refs/trakkin/incoming"),
            ],
            None,
            None,
        )
        .await?;
        self.run(
            &[
                "-c",
                "gc.auto=6700",
                "-c",
                "gc.autoDetach=false",
                "gc",
                "--auto",
            ],
            None,
            None,
        )
        .await?;
        Ok(self
            .text(
                &["rev-parse", "--verify", "refs/trakkin/incoming^{commit}"],
                None,
                None,
            )
            .await?
            .trim()
            .to_owned())
    }
}

impl History {
    pub async fn publication_remote(&self, remote: &str, branch: &str) -> Result<Option<String>> {
        let remote = validate_remote(remote)?;
        self.publication_remote_validated(&remote, branch).await
    }
    async fn publication_remote_validated(
        &self,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>> {
        validate_branch(branch)?;
        self.run(
            &["check-ref-format", &format!("refs/heads/{branch}")],
            None,
            None,
        )
        .await?;
        let advertised = self
            .text(
                &[
                    "ls-remote",
                    "--refs",
                    remote,
                    &format!("refs/heads/{branch}"),
                ],
                None,
                None,
            )
            .await?;
        if advertised.is_empty() {
            return Ok(None);
        }
        let (commit, reference) = advertised
            .trim()
            .split_once('\t')
            .context("invalid publication branch advertisement")?;
        validate_commit(commit)?;
        ensure!(
            reference == format!("refs/heads/{branch}"),
            "ambiguous publication branch"
        );
        self.run(&["config", "remote.publication.url", remote], None, None)
            .await?;
        self.run(
            &["config", "remote.publication.promisor", "true"],
            None,
            None,
        )
        .await?;
        self.run(
            &[
                "config",
                "remote.publication.partialclonefilter",
                "blob:none",
            ],
            None,
            None,
        )
        .await?;
        self.run(
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-auto-maintenance",
                "--depth=1",
                "--filter=blob:none",
                "publication",
                &format!("+refs/heads/{branch}:refs/trakkin/publication-remote"),
            ],
            None,
            None,
        )
        .await?;
        Ok(Some(
            self.text(
                &[
                    "rev-parse",
                    "--verify",
                    "refs/trakkin/publication-remote^{commit}",
                ],
                None,
                None,
            )
            .await?
            .trim()
            .to_owned(),
        ))
    }
    pub async fn follow_publication(&self, commit: &str) -> Result<()> {
        validate_commit(commit)?;
        let _guard = self.lock()?;
        self.run(
            &["update-ref", "HEAD", commit, &self.head().await?],
            None,
            None,
        )
        .await?;
        Ok(())
    }
    pub async fn push_publication(
        &self,
        remote: &str,
        branch: &str,
        expected: Option<&str>,
        commit: &str,
    ) -> Result<()> {
        let remote = validate_remote(remote)?;
        self.push_publication_validated(&remote, branch, expected, commit)
            .await
    }
    async fn push_publication_validated(
        &self,
        remote: &str,
        branch: &str,
        expected: Option<&str>,
        commit: &str,
    ) -> Result<()> {
        validate_branch(branch)?;
        validate_commit(commit)?;
        if let Some(expected) = expected {
            validate_commit(expected)?;
        }
        self.run(
            &[
                "push",
                "--porcelain",
                &format!(
                    "--force-with-lease=refs/heads/{branch}:{}",
                    expected.unwrap_or("")
                ),
                remote,
                &format!("{commit}:refs/heads/{branch}"),
            ],
            None,
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn prepare(
        &self,
        expected: &str,
        changes: &BTreeMap<String, Option<String>>,
        message: &str,
    ) -> Result<Prepared, WriteError> {
        validate_commit(expected)?;
        if changes.len() > 1000 {
            return Err(anyhow::anyhow!("publication batch exceeds 1000 files").into());
        }
        let _guard = self.lock()?;
        if self.head().await? != expected {
            return Err(WriteError::RevisionChanged);
        }
        self.safe_config().await?;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_secs();
        let date = format!("@{seconds} +0000");
        let commit = self
            .create_commit(expected, changes, message, &date)
            .await?;
        self.run(
            &["update-ref", "refs/trakkin/prepared", &commit],
            None,
            None,
        )
        .await?;
        Ok(Prepared {
            base: expected.to_owned(),
            commit,
            changes: changes.clone(),
            message: message.to_owned(),
            date,
        })
    }

    async fn create_commit(
        &self,
        expected: &str,
        changes: &BTreeMap<String, Option<String>>,
        message: &str,
        date: &str,
    ) -> Result<String> {
        let directory = tempfile::tempdir().context("cannot prepare publication index")?;
        let index = directory.path().join("index");
        self.run(&["read-tree", expected], None, Some(&index))
            .await?;
        let mut updates = Vec::new();
        for (path, text) in changes {
            validate_path(path)?;
            let object = if let Some(text) = text {
                validate_file(path, text)?;
                self.text(
                    &["hash-object", "-w", "--stdin"],
                    Some(text.as_bytes()),
                    None,
                )
                .await?
                .trim()
                .to_owned()
            } else {
                "0".repeat(expected.len())
            };
            updates.extend_from_slice(
                format!(
                    "{} {object}\t{path}\0",
                    if text.is_some() { "100644" } else { "0" }
                )
                .as_bytes(),
            );
        }
        self.run(
            &["update-index", "-z", "--index-info"],
            Some(&updates),
            Some(&index),
        )
        .await?;
        let tree = self.text(&["write-tree"], None, Some(&index)).await?;
        let current_tree = self
            .text(&["rev-parse", &format!("{expected}^{{tree}}")], None, None)
            .await?;
        if tree == current_tree {
            return Ok(expected.to_owned());
        }
        let mut command = self.command(
            &["commit-tree", tree.trim(), "-p", expected, "-m", message],
            None,
            None,
        )?;
        command
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date);
        Ok(String::from_utf8(self.execute(command, None).await?)?
            .trim()
            .to_owned())
    }

    pub async fn restore(&self, prepared: &Prepared) -> Result<()> {
        validate_commit(&prepared.base)?;
        validate_commit(&prepared.commit)?;
        if self
            .run(
                &["cat-file", "-e", &format!("{}^{{commit}}", prepared.commit)],
                None,
                None,
            )
            .await
            .is_err()
        {
            ensure!(
                prepared.changes.len() <= 1000,
                "publication batch exceeds 1000 files"
            );
            let commit = self
                .create_commit(
                    &prepared.base,
                    &prepared.changes,
                    &prepared.message,
                    &prepared.date,
                )
                .await?;
            ensure!(
                commit == prepared.commit,
                "stored publication does not reproduce its commit"
            );
            self.run(
                &["update-ref", "refs/trakkin/prepared", &commit],
                None,
                None,
            )
            .await?;
        }
        Ok(())
    }
    pub async fn apply(&self, prepared: &Prepared) -> Result<(), WriteError> {
        validate_commit(&prepared.base)?;
        validate_commit(&prepared.commit)?;
        self.restore(prepared).await?;
        let _guard = self.lock()?;
        let current = self.head().await?;
        if current != prepared.base && current != prepared.commit {
            return Err(WriteError::RevisionChanged);
        }
        if current == prepared.base {
            self.safe_config().await?;
            self.run(
                &["update-ref", "HEAD", &prepared.commit, &prepared.base],
                None,
                None,
            )
            .await?;
        }
        Ok(())
    }
}

impl Repository {
    fn lock(&self) -> Result<RepositoryLock> {
        let path = self.path.with_extension("git.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("repository is busy; retry the operation")?;
        Ok(RepositoryLock(file))
    }
    async fn safe_config(&self) -> Result<()> {
        let configuration = self
            .run(
                &["config", "--local", "--no-includes", "--null", "--list"],
                None,
                None,
            )
            .await?;
        for entry in configuration.split(|byte| *byte == 0) {
            let key = entry
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default();
            ensure!(
                !key.starts_with(b"filter.")
                    && !key.starts_with(b"include.")
                    && !key.starts_with(b"includeif."),
                "repository filters and configuration includes are not allowed"
            );
        }
        Ok(())
    }
    fn command(
        &self,
        arguments: &[&str],
        input: Option<&[u8]>,
        index: Option<&Path>,
    ) -> Result<Command> {
        let mut command = Command::new("git");
        command.current_dir(if self.path.exists() {
            &self.path
        } else {
            self.path.parent().context("repository has no parent")?
        });
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("GIT_"))
        {
            command.env_remove(key);
        }
        if matches!(self.credentials, Credentials::Publisher)
            && let Ok(authorization) = std::env::var("TRAKKIN_MAPPINGS_GIT_AUTHORIZATION")
        {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
                .env(
                    "GIT_CONFIG_VALUE_0",
                    format!("AUTHORIZATION: {authorization}"),
                );
        }
        command.args([
            "--no-pager",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.attributesFile=",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "credential.helper=",
            "-c",
            "credential.interactive=false",
            "-c",
            "user.name=Trakkin",
            "-c",
            "user.email=trakkin@localhost",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.http.allow=always",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "gc.auto=0",
            "-c",
            "http.lowSpeedLimit=1",
            "-c",
            "http.lowSpeedTime=30",
        ]);
        #[cfg(test)]
        command.args(["-c", "protocol.file.allow=always"]);
        command
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        Ok(command)
    }
    async fn run(
        &self,
        arguments: &[&str],
        input: Option<&[u8]>,
        index: Option<&Path>,
    ) -> Result<Vec<u8>> {
        self.execute(self.command(arguments, input, index)?, input)
            .await
    }
    async fn execute(&self, mut command: Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut child: Child = command.spawn().context("Git must be installed")?;
        tokio::time::timeout(COMMAND_TIMEOUT, async {
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().context("Git output unavailable")?;
            let stderr = child.stderr.take().context("Git diagnostics unavailable")?;
            let writer = async move {
                if let Some(input) = input {
                    stdin
                        .context("Git input unavailable")?
                        .write_all(input)
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let (stdout, stderr, status, ()) = tokio::try_join!(
                async { Ok::<_, anyhow::Error>(read_output(stdout).await?) },
                async { Ok::<_, anyhow::Error>(read_output(stderr).await?) },
                async { Ok::<_, anyhow::Error>(child.wait().await?) },
                writer
            )?;
            ensure!(
                status.success(),
                "Git operation failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            );
            Ok(stdout)
        })
        .await
        .context("Git operation timed out")?
    }
    async fn text(
        &self,
        arguments: &[&str],
        input: Option<&[u8]>,
        index: Option<&Path>,
    ) -> Result<String> {
        Ok(String::from_utf8(self.run(arguments, input, index).await?)?)
    }
}

pub fn validate_remote(remote: &str) -> Result<String> {
    ensure!(
        remote.len() <= 2048 && !remote.chars().any(char::is_control),
        "invalid Git repository URL"
    );
    let url = url::Url::parse(remote)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Git repository requires an HTTP(S) URL without credentials, query or fragment"
    );
    Ok(url.to_string())
}
fn validate_branch(branch: &str) -> Result<()> {
    ensure!(
        !branch.is_empty()
            && branch.len() <= 128
            && !branch
                .chars()
                .any(|character| character.is_whitespace() || character.is_control()),
        "invalid Git branch"
    );
    Ok(())
}
fn validate_commit(commit: &str) -> Result<()> {
    ensure!(
        [40, 64].contains(&commit.len())
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "revision must be a full lowercase Git commit ID"
    );
    Ok(())
}
fn validate_path(path: &str) -> Result<()> {
    if path == POLICY {
        return Ok(());
    }
    let Some((shard, name)) = path
        .strip_prefix("mappings/v1/claims/")
        .and_then(|path| path.split_once('/'))
    else {
        anyhow::bail!("unexpected mapping path {path}");
    };
    let hash = name
        .strip_suffix(".trk")
        .context("invalid claim filename")?;
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && shard == &hash[..2],
        "invalid claim path"
    );
    Ok(())
}
fn validate_file(path: &str, text: &str) -> Result<()> {
    ensure!(text.len() <= FILE_LIMIT, "mapping file exceeds 2 MiB");
    if path == POLICY {
        ConflictPolicy::parse(text)?;
    } else {
        Artifact::parse(path, text)?;
    }
    Ok(())
}
async fn read_output(reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    if output.len() > OUTPUT_LIMIT {
        return Err(std::io::Error::other(
            "Git output exceeds bounded operation limit",
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_record;

    #[tokio::test]
    async fn delta_streams_bounded_batches_without_restarting() {
        let directory = tempfile::tempdir().unwrap();
        let repository = History::open(&directory.path().join("history.git"))
            .await
            .unwrap();
        let mut expected = BTreeMap::new();
        let mut artifacts = (0..1010)
            .map(|number| Artifact {
                record: parse_record(&format!("a://{number} <=> b://{number}")).unwrap(),
                rejected: false,
            })
            .collect::<Vec<_>>();
        artifacts.sort_by_key(Artifact::path);
        for (number, mut artifact) in artifacts.into_iter().enumerate() {
            if number < 20 {
                artifact
                    .record
                    .metadata
                    .push(format!("#@note {}", "x".repeat(1800 * 1024)));
            }
            expected.insert(artifact.path(), Some(artifact.text()));
        }
        let entries = expected.clone().into_iter().collect::<Vec<_>>();
        for chunk in entries.chunks(1000) {
            let prepared = repository
                .prepare(
                    &repository.head().await.unwrap(),
                    &chunk.iter().cloned().collect(),
                    "Fixture batch",
                )
                .await
                .unwrap();
            repository.apply(&prepared).await.unwrap();
        }
        let mut delta = repository
            .delta(None, &repository.head().await.unwrap())
            .await
            .unwrap();
        let mut observed = BTreeMap::new();
        let mut paths = Vec::new();
        let mut limits = [1000, 8, 3].into_iter().cycle();
        loop {
            let limit = limits.next().unwrap();
            let batch = delta.next_batch(limit).await.unwrap();
            if batch.is_empty() {
                break;
            }
            assert!(batch.len() <= limit);
            assert!(
                batch
                    .iter()
                    .filter_map(|change| change.text.as_ref())
                    .map(String::len)
                    .sum::<usize>()
                    <= OUTPUT_LIMIT
            );
            for change in batch {
                paths.push(change.path.clone());
                assert!(observed.insert(change.path, change.text).is_none());
            }
        }
        assert!(paths.iter().eq(expected.keys()));
        assert_eq!(observed, expected);
        let unchanged = repository
            .prepare(
                &repository.head().await.unwrap(),
                &BTreeMap::new(),
                "Reader released its lock",
            )
            .await
            .unwrap();
        assert_eq!(unchanged.commit, repository.head().await.unwrap());
    }

    #[tokio::test]
    async fn upstream_cache_bounds_pins_and_reclaims_expired_history() {
        let directory = tempfile::tempdir().unwrap();
        let remote = History::open(&directory.path().join("remote.git"))
            .await
            .unwrap();
        let store = RepositoryStore::open(&directory.path().join("consumer")).unwrap();
        let cache = store.upstream("community").unwrap();
        let mut commits = Vec::new();
        let mut applied = None;
        for number in 0..12 {
            let artifact = Artifact {
                record: parse_record(&format!("#@note Revision {number}\na://1 <=> b://1\n"))
                    .unwrap(),
                rejected: false,
            };
            let prepared = remote
                .prepare(
                    &remote.head().await.unwrap(),
                    &BTreeMap::from([(artifact.path(), Some(artifact.text()))]),
                    "Update fixture mapping",
                )
                .await
                .unwrap();
            remote.apply(&prepared).await.unwrap();
            let commit = cache
                .fetch_validated(remote.path().to_str().unwrap(), "main", applied.as_deref())
                .await
                .unwrap();
            assert_eq!(commit, prepared.commit);
            if number < 8 {
                applied = Some(commit.clone());
            }
            commits.push(commit);
        }
        let current = commits.last().unwrap();
        let previous = &commits[7];
        assert_eq!(
            cache
                .text(
                    &["rev-parse", "refs/trakkin/incoming", CACHE_APPLIED],
                    None,
                    None
                )
                .await
                .unwrap(),
            format!("{current}\n{previous}\n")
        );
        assert_eq!(
            cache
                .text(
                    &["for-each-ref", "--format=%(refname)", "refs/trakkin/"],
                    None,
                    None
                )
                .await
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(cache.contains_commit(&commits[0]).await.unwrap());
        let old = std::time::SystemTime::now() - Duration::from_secs(21 * 86400);
        for directory in fs::read_dir(cache.path().join("objects")).unwrap() {
            let directory = directory.unwrap();
            if directory.file_type().unwrap().is_dir() {
                for object in fs::read_dir(directory.path()).unwrap() {
                    let object = object.unwrap();
                    if object.file_type().unwrap().is_file() {
                        fs::File::open(object.path())
                            .unwrap()
                            .set_modified(old)
                            .unwrap();
                    }
                }
            }
        }
        assert_eq!(
            cache
                .fetch_validated(remote.path().to_str().unwrap(), "main", applied.as_deref())
                .await
                .unwrap(),
            *current
        );
        cache
            .run(&["gc", "--quiet", "--prune=2.weeks.ago"], None, None)
            .await
            .unwrap();
        assert!(!cache.contains_commit(&commits[0]).await.unwrap());
        assert!(!cache.contains_commit(&commits[8]).await.unwrap());
        assert!(cache.contains_commit(previous).await.unwrap());
        assert_eq!(
            cache
                .delta(None, current)
                .await
                .unwrap()
                .next_batch(1000)
                .await
                .unwrap()
                .len(),
            1
        );
        remote
            .run(&["gc", "--quiet", "--prune=now"], None, None)
            .await
            .unwrap();
        assert!(remote.contains_commit(&commits[0]).await.unwrap());
    }

    #[tokio::test]
    async fn delta_pages_only_read_changed_claims_and_replay_prepared_commits() {
        let directory = tempfile::tempdir().unwrap();
        let repository = History::open(&directory.path().join("local"))
            .await
            .unwrap();
        let base = repository.head().await.unwrap();
        let artifacts = (1..=3)
            .map(|number| Artifact {
                record: parse_record(&format!("a://{number} <=> b://{number}\n")).unwrap(),
                rejected: false,
            })
            .collect::<Vec<_>>();
        let changes = artifacts
            .iter()
            .map(|artifact| (artifact.path(), Some(artifact.text())))
            .collect();
        let prepared = repository
            .prepare(&base, &changes, "Initial decisions")
            .await
            .unwrap();
        assert_eq!(repository.head().await.unwrap(), base);
        assert!(repository.contains_commit(&base).await.unwrap());
        assert!(
            !repository
                .contains_commit(&"0".repeat(base.len()))
                .await
                .unwrap()
        );
        repository.apply(&prepared).await.unwrap();
        repository.apply(&prepared).await.unwrap();
        let mut reader = repository.delta(None, &prepared.commit).await.unwrap();
        assert_eq!(reader.next_batch(2).await.unwrap().len(), 2);
        assert_eq!(reader.next_batch(2).await.unwrap().len(), 1);
        assert!(reader.next_batch(2).await.unwrap().is_empty());
        let changes = BTreeMap::from([(artifacts[0].path(), None)]);
        let second = repository
            .prepare(&prepared.commit, &changes, "Delete one decision")
            .await
            .unwrap();
        repository.apply(&second).await.unwrap();
        let delta = repository
            .delta(Some(&prepared.commit), &second.commit)
            .await
            .unwrap()
            .next_batch(10)
            .await
            .unwrap();
        assert_eq!(delta.len(), 1);
        assert!(delta[0].text.is_none());
        let unchanged = repository
            .prepare(&second.commit, &BTreeMap::new(), "Unchanged")
            .await
            .unwrap();
        assert_eq!(unchanged.commit, second.commit);
        assert!(repository.path().join("HEAD").is_file());
        assert!(!repository.path().join(".git").exists());
    }

    #[tokio::test]
    async fn pending_publications_survive_cache_loss_and_remote_pushes_use_a_lease() {
        let directory = tempfile::tempdir().unwrap();
        let cache = directory.path().join("cache.git");
        let repository = History::open(&cache).await.unwrap();
        let remote = History::open(&directory.path().join("remote.git"))
            .await
            .unwrap();
        let artifact = Artifact {
            record: parse_record("a://1 <=> b://1\n").unwrap(),
            rejected: false,
        };
        let prepared = repository
            .prepare(
                &repository.head().await.unwrap(),
                &BTreeMap::from([(artifact.path(), Some(artifact.text()))]),
                "Durable publication",
            )
            .await
            .unwrap();
        let stored = serde_json::to_vec(&prepared).unwrap();
        drop(repository);
        fs::remove_dir_all(&cache).unwrap();
        let recovered = History::open(&cache).await.unwrap();
        let prepared: Prepared = serde_json::from_slice(&stored).unwrap();
        recovered.apply(&prepared).await.unwrap();
        assert_eq!(recovered.head().await.unwrap(), prepared.commit);
        recovered
            .push_publication_validated(
                remote.path().to_str().unwrap(),
                "mappings",
                None,
                &prepared.commit,
            )
            .await
            .unwrap();
        assert!(
            recovered
                .push_publication_validated(
                    remote.path().to_str().unwrap(),
                    "mappings",
                    None,
                    &prepared.base
                )
                .await
                .is_err()
        );
        assert_eq!(
            recovered
                .publication_remote_validated(remote.path().to_str().unwrap(), "mappings")
                .await
                .unwrap(),
            Some(prepared.commit)
        );
    }
}
