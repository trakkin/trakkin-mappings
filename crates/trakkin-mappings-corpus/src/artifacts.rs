use crate::{adapters::Adapters, inventory, lock, read_shard};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Command,
};

pub const INDEX: &str = ".cache/trakkin-mappings/index-v1.sqlite";

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub rebuilt_shards: usize,
    pub removed_shards: usize,
    pub reused_shards: usize,
    pub mappings: u64,
    pub warnings: Vec<String>,
}

pub fn index(root: &Path, database: &Path, adapters: &Adapters) -> Result<IndexReport> {
    let _lock = lock(root)?;
    index_unlocked(root, database, adapters)
}

fn index_unlocked(root: &Path, database: &Path, adapters: &Adapters) -> Result<IndexReport> {
    let mut connection = trakkin_mappings_index::open(database)?;
    let previous = trakkin_mappings_index::shard_keys(&connection)?;
    let adapter_hash = trakkin_mappings_language::digest(&[
        b"trakkin:validation-cache:v1\0",
        adapters.fingerprint().as_bytes(),
        include_bytes!("../../../Cargo.toml"),
        include_bytes!("../../../Cargo.lock"),
        include_bytes!("../../../grammar/Trakkin.g4"),
        include_bytes!("../../trakkin-mappings-language/Cargo.toml"),
        include_bytes!("../../trakkin-mappings-language/build.rs"),
        include_bytes!("../../trakkin-mappings-language/src/lib.rs"),
        include_bytes!("../../trakkin-mappings-language/src/parser.rs"),
        include_bytes!("../../trakkin-mappings-language/src/validation.rs"),
        include_bytes!("../../trakkin-mappings-index/Cargo.toml"),
        include_bytes!("../../trakkin-mappings-index/src/lib.rs"),
        include_bytes!("../Cargo.toml"),
        include_bytes!("adapters.rs"),
        include_bytes!("lib.rs"),
        include_bytes!("artifacts.rs"),
    ]);
    let mut current = BTreeMap::new();
    for path in inventory(root)? {
        let hash = hash_file(&root.join(&path))?;
        current.insert(
            path.to_str().context("non UTF-8 shard path")?.to_owned(),
            hash,
        );
    }
    let changed: Vec<_> = current
        .iter()
        .filter(|(path, hash)| {
            previous.get(*path) != Some(&((*hash).clone(), adapter_hash.clone()))
        })
        .collect();
    let removed: Vec<_> = previous
        .keys()
        .filter(|path| !current.contains_key(*path))
        .collect();
    let transaction = connection.transaction()?;
    for path in changed
        .iter()
        .map(|(path, _)| *path)
        .chain(removed.iter().copied())
    {
        transaction.execute("DELETE FROM shard WHERE path = ?1", [path])?;
    }
    for (path, hash) in &changed {
        transaction.execute(
            "INSERT INTO shard VALUES (?1, ?2, ?3)",
            params![path, hash, adapter_hash],
        )?;
        for record in read_shard(root, Path::new(path), adapters)? {
            let resolved = trakkin_mappings_language::validate(&record.statement, adapters)?;
            let left = record.statement.left.selections()[0].source();
            let right = record.statement.right.selections()[0].source();
            trakkin_mappings_index::insert_record(
                &transaction,
                path,
                &record,
                &resolved,
                (
                    adapters.exclusive(left, right),
                    adapters.exclusive(right, left),
                ),
            )?;
        }
    }
    trakkin_mappings_index::check_conflicts(&transaction)?;
    let mappings = transaction.query_row("SELECT count(*) FROM mapping", [], |row| row.get(0))?;
    let warnings = trakkin_mappings_index::overlap_warnings(&transaction)?;
    transaction.commit()?;
    Ok(IndexReport {
        rebuilt_shards: changed.len(),
        removed_shards: removed.len(),
        reused_shards: current.len() - changed.len(),
        mappings,
        warnings,
    })
}

fn hash_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[derive(Debug, Serialize)]
pub struct Asset {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    pub format_version: u32,
    pub canonicalization: String,
    pub source_commit: String,
    pub dirty: bool,
    pub adapter_sha256: String,
    pub logical_sha256: String,
    pub mappings: u64,
    pub shards: usize,
    pub streams: BTreeMap<String, Vec<String>>,
    pub assets: Vec<Asset>,
}

pub fn git_provenance(root: &Path, allow_dirty: bool) -> Result<(String, bool)> {
    let revision = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()?;
    ensure!(
        revision.status.success(),
        "release requires a Git checkout with HEAD"
    );
    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()?;
    ensure!(status.status.success(), "cannot determine Git status");
    let dirty = !status.stdout.is_empty();
    ensure!(
        !dirty || allow_dirty,
        "release requires a clean checkout; --allow-dirty is for local testing only"
    );
    Ok((String::from_utf8(revision.stdout)?.trim().to_owned(), dirty))
}

pub fn release(
    root: &Path,
    output: &Path,
    adapters: &Adapters,
    commit: &str,
    dirty: bool,
    part_bytes: u64,
) -> Result<Manifest> {
    ensure!(
        !output.exists(),
        "release output already exists: {}",
        output.display()
    );
    ensure!(
        part_bytes > 0 && part_bytes < 2_147_483_648,
        "part size must be between 1 byte and GitHub's 2 GiB limit"
    );
    let _lock = lock(root)?;
    let database = root.join(INDEX);
    let report = index_unlocked(root, &database, adapters)?;
    let cached = trakkin_mappings_index::open(&database)?;
    let logical_hash = trakkin_mappings_index::logical_hash(&cached)?;
    drop(cached);
    let scratch = tempfile::tempdir()?;
    let clean_path = scratch.path().join("clean.sqlite");
    index_unlocked(root, &clean_path, adapters)?;
    let clean = trakkin_mappings_index::open(&clean_path)?;
    ensure!(
        logical_hash == trakkin_mappings_index::logical_hash(&clean)?,
        "incremental index differs from clean build; discard the derived cache and investigate"
    );
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stage = tempfile::tempdir_in(parent)?;
    let sqlite = stage.path().join("trakkin-v1.sqlite");
    trakkin_mappings_index::snapshot(&clean_path, &sqlite, commit, dirty, &adapters.fingerprint())?;
    let sqlite_compressed = stage.path().join("trakkin-v1.sqlite.zst");
    let mut compressed = zstd::stream::write::Encoder::new(File::create(&sqlite_compressed)?, 9)?;
    io::copy(&mut File::open(&sqlite)?, &mut compressed)?;
    compressed.finish()?.sync_all()?;
    fs::remove_file(sqlite)?;
    let text_compressed = stage.path().join("trakkin-v1.trakkin.zst");
    let mut compressed = zstd::stream::write::Encoder::new(File::create(&text_compressed)?, 9)?;
    let mut statement = clean.prepare("SELECT record FROM mapping ORDER BY shard, id")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let record: String = row.get(0)?;
        compressed.write_all(record.as_bytes())?;
    }
    compressed.finish()?.sync_all()?;
    let mut streams = BTreeMap::new();
    let mut assets = Vec::new();
    for path in [sqlite_compressed, text_compressed] {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let parts = split_asset(&path, part_bytes)?;
        streams.insert(
            name,
            parts
                .iter()
                .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
                .collect(),
        );
        for part in parts {
            assets.push(Asset {
                name: part.file_name().unwrap().to_str().unwrap().to_owned(),
                bytes: fs::metadata(&part)?.len(),
                sha256: hash_file(&part)?,
            });
        }
    }
    assets.sort_by(|left, right| left.name.cmp(&right.name));
    let manifest = Manifest {
        format_version: 1,
        canonicalization: "v1".into(),
        source_commit: commit.into(),
        dirty,
        adapter_sha256: adapters.fingerprint(),
        logical_sha256: logical_hash,
        mappings: report.mappings,
        shards: report.rebuilt_shards + report.reused_shards,
        streams,
        assets,
    };
    let manifest_path = stage.path().join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    let mut checksums = format!("{}  manifest.json\n", hash_file(&manifest_path)?);
    for asset in &manifest.assets {
        checksums.push_str(&format!("{}  {}\n", asset.sha256, asset.name));
    }
    fs::write(stage.path().join("SHA256SUMS"), checksums)?;
    fs::rename(stage.path(), output)?;
    Ok(manifest)
}

fn split_asset(path: &Path, limit: u64) -> Result<Vec<PathBuf>> {
    if fs::metadata(path)?.len() <= limit {
        return Ok(vec![path.to_owned()]);
    }
    let mut input = File::open(path)?;
    let mut parts = Vec::new();
    let length = input.metadata()?.len();
    for index in 0..length.div_ceil(limit) {
        let destination = PathBuf::from(format!("{}.part{:05}", path.display(), index + 1));
        let mut output = File::create(&destination)?;
        io::copy(&mut (&mut input).take(limit), &mut output)?;
        output.sync_all()?;
        parts.push(destination);
    }
    fs::remove_file(path)?;
    Ok(parts)
}
