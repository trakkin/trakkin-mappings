pub mod adapters;
pub mod artifacts;
pub mod index;

use adapters::Adapters;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use trakkin_mappings_language::{Record, parse, validate};

pub const ADAPTERS: &str = "mappings/v1/adapters.json";
pub const CORPUS: &str = "mappings/v1";

pub fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn shard_path(id: &str) -> Result<PathBuf> {
    ensure!(
        valid_id(id),
        "expected a lowercase 64-character SHA-256 mapping ID"
    );
    Ok(Path::new(CORPUS)
        .join(&id[..2])
        .join(format!("{}.trakkin", &id[..3])))
}

pub fn is_shard(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(CORPUS) else {
        return false;
    };
    let components: Vec<_> = relative.iter().collect();
    if components.len() != 2 {
        return false;
    }
    let directory = components[0].to_string_lossy();
    let file = components[1].to_string_lossy();
    let Some(prefix) = file.strip_suffix(".trakkin") else {
        return false;
    };
    prefix.len() == 3
        && directory.len() == 2
        && prefix.starts_with(directory.as_ref())
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn lock(root: &Path) -> Result<File> {
    let directory = root.join(".cache/trakkin-mappings");
    fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("write.lock"))?;
    file.try_lock_exclusive()
        .context("another trakkin-mappings writer is running")?;
    Ok(file)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("output needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

fn safe_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        is_shard(relative),
        "unexpected corpus path {}",
        relative.display()
    );
    let mut path = root.to_owned();
    for component in relative.components() {
        path.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "symlinks are not allowed in the corpus: {}",
                path.display()
            );
        }
    }
    Ok(path)
}

pub fn inventory(root: &Path) -> Result<Vec<PathBuf>> {
    fn visit(root: &Path, relative: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
        let full = root.join(relative);
        let metadata = fs::symlink_metadata(&full)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symlink in corpus: {}",
            relative.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(full)? {
                visit(root, &relative.join(entry?.file_name()), paths)?;
            }
        } else if relative == Path::new(ADAPTERS) {
            ensure!(metadata.is_file(), "invalid corpus metadata");
        } else {
            ensure!(
                metadata.is_file() && is_shard(relative),
                "unexpected corpus file {}",
                relative.display()
            );
            paths.push(relative.to_owned());
        }
        Ok(())
    }
    let mut paths = Vec::new();
    safe_path(root, &Path::new(CORPUS).join("00/000.trakkin"))?;
    if root.join("mappings").try_exists()? {
        visit(root, Path::new("mappings"), &mut paths)?;
    }
    paths.sort();
    Ok(paths)
}

pub fn read_shard(root: &Path, relative: &Path, adapters: &Adapters) -> Result<Vec<Record>> {
    let path = safe_path(root, relative)?;
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path)?;
    ensure!(
        !text.is_empty(),
        "empty shards must be removed: {}",
        relative.display()
    );
    let records = parse(&text).with_context(|| relative.display().to_string())?;
    let mut previous: Option<String> = None;
    let mut canonical = String::new();
    for record in &records {
        let id = record.statement.id();
        ensure!(
            shard_path(&id)? == relative,
            "wrong shard for {id}: {}",
            relative.display()
        );
        ensure!(
            previous.as_ref().is_none_or(|previous| previous < &id),
            "duplicate or unsorted mapping {id}"
        );
        validate_record(record, adapters)
            .with_context(|| format!("{id} in {}", relative.display()))?;
        canonical.push_str(&record.corpus_form()?.canonical());
        previous = Some(id);
    }
    ensure!(
        canonical == text,
        "noncanonical shard {}; use trakkin-mappings fmt to inspect canonical input, then reinsert",
        relative.display()
    );
    Ok(records)
}

pub fn validate_record(record: &Record, adapters: &Adapters) -> Result<()> {
    record.corpus_form()?;
    for expression in [&record.statement.left, &record.statement.right] {
        let sources: BTreeSet<_> = expression
            .selections()
            .iter()
            .map(|selection| selection.source())
            .collect();
        ensure!(
            sources.len() == 1,
            "corpus requires one source namespace per logical side"
        );
    }
    validate(&record.statement, adapters)?;
    Ok(())
}

pub fn insert(
    root: &Path,
    input: &str,
    adapters: &Adapters,
    replace_metadata: bool,
) -> Result<usize> {
    let _lock = lock(root)?;
    let records = parse(input)?
        .into_iter()
        .map(|record| record.corpus_form())
        .collect::<Result<Vec<_>>>()?;
    let mut shards = BTreeMap::<PathBuf, BTreeMap<String, Record>>::new();
    let mut changed = BTreeSet::new();
    let mut count = 0;
    for record in records {
        validate_record(&record, adapters)?;
        let id = record.statement.id();
        let relative = shard_path(&id)?;
        if !shards.contains_key(&relative) {
            let existing = read_shard(root, &relative, adapters)?
                .into_iter()
                .map(|record| (record.statement.id(), record))
                .collect();
            shards.insert(relative.clone(), existing);
        }
        if record.statement.operator != trakkin_mappings_language::Operator::Implication {
            let reversed = trakkin_mappings_language::Statement {
                left: record.statement.right.clone(),
                right: record.statement.left.clone(),
                operator: record.statement.operator,
            };
            let reverse_id = reversed.id();
            if reverse_id != id {
                let reverse_path = shard_path(&reverse_id)?;
                if !shards.contains_key(&reverse_path) {
                    let records = read_shard(root, &reverse_path, adapters)?
                        .into_iter()
                        .map(|record| (record.statement.id(), record))
                        .collect();
                    shards.insert(reverse_path.clone(), records);
                }
                ensure!(
                    !shards[&reverse_path].contains_key(&reverse_id),
                    "reversed duplicate already exists: {reverse_id}"
                );
            }
        }
        let shard = shards.get_mut(&relative).unwrap();
        if let Some(existing) = shard.get(&id) {
            ensure!(
                existing.statement.canonical() == record.statement.canonical(),
                "fatal mapping ID collision"
            );
            if existing == &record {
                continue;
            }
            ensure!(
                replace_metadata,
                "mapping {id} exists with different annotations; use --replace-metadata"
            );
        }
        shard.insert(id, record);
        changed.insert(relative);
        count += 1;
    }
    for relative in changed {
        let text: String = shards[&relative].values().map(Record::canonical).collect();
        atomic_write(&safe_path(root, &relative)?, text.as_bytes())?;
    }
    Ok(count)
}

pub fn remove(root: &Path, id: &str, adapters: &Adapters) -> Result<bool> {
    let _lock = lock(root)?;
    let relative = shard_path(id)?;
    let mut records = read_shard(root, &relative, adapters)?;
    let count = records.len();
    records.retain(|record| record.statement.id() != id);
    if records.len() == count {
        return Ok(false);
    }
    let path = safe_path(root, &relative)?;
    if records.is_empty() {
        fs::remove_file(path)?;
    } else {
        atomic_write(
            &path,
            records
                .iter()
                .map(Record::canonical)
                .collect::<String>()
                .as_bytes(),
        )?;
    }
    Ok(true)
}

pub fn changed_shards(root: &Path, base: &str) -> Result<Vec<PathBuf>> {
    ensure!(!base.starts_with('-'), "invalid base revision");
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            "HEAD",
            "--",
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "git diff failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let paths: Vec<_> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| String::from_utf8(bytes.to_vec()).map(PathBuf::from))
        .collect::<std::result::Result<_, _>>()?;
    if paths.iter().any(|path| {
        path.starts_with("crates")
            || path == Path::new(ADAPTERS)
            || path == Path::new("SPEC.md")
            || path == Path::new("Cargo.toml")
            || path == Path::new("Cargo.lock")
            || path.starts_with("grammar")
    }) {
        return inventory(root);
    }
    let mut shards = Vec::new();
    for path in paths {
        if path.starts_with("mappings") {
            ensure!(
                is_shard(&path),
                "unexpected mapping path {}",
                path.display()
            );
            shards.push(path);
        }
    }
    Ok(shards)
}

#[derive(Debug, Serialize)]
pub struct Statistics {
    pub mappings: usize,
    pub shards: usize,
    pub bytes: u64,
    pub largest_shard_bytes: u64,
    pub largest_shard_records: usize,
    pub warnings: Vec<String>,
}

pub fn statistics(root: &Path) -> Result<Statistics> {
    let paths = inventory(root)?;
    let mut result = Statistics {
        mappings: 0,
        shards: paths.len(),
        bytes: 0,
        largest_shard_bytes: 0,
        largest_shard_records: 0,
        warnings: Vec::new(),
    };
    for path in paths {
        let text = fs::read_to_string(root.join(path))?;
        let count = text.lines().filter(|line| !line.starts_with('#')).count();
        result.mappings += count;
        result.bytes += text.len() as u64;
        result.largest_shard_bytes = result.largest_shard_bytes.max(text.len() as u64);
        result.largest_shard_records = result.largest_shard_records.max(count);
    }
    if result.largest_shard_bytes >= 1_048_576 || result.largest_shard_records >= 2500 {
        result
            .warnings
            .push("shard growth threshold reached: plan a versioned layout migration".into());
    }
    Ok(result)
}

pub fn validate_paths(root: &Path, paths: &[PathBuf], adapters: &Adapters) -> Result<usize> {
    let mut records = 0;
    let mut checked = BTreeMap::<PathBuf, BTreeSet<String>>::new();
    for path in paths {
        let shard = read_shard(root, path, adapters)?;
        records += shard.len();
        for record in shard {
            if record.statement.operator == trakkin_mappings_language::Operator::Implication {
                continue;
            }
            let reversed = trakkin_mappings_language::Statement {
                left: record.statement.right.clone(),
                right: record.statement.left.clone(),
                operator: record.statement.operator,
            };
            let reverse_id = reversed.id();
            if reverse_id == record.statement.id() {
                continue;
            }
            let reverse_path = shard_path(&reverse_id)?;
            if !checked.contains_key(&reverse_path) {
                let ids = read_shard(root, &reverse_path, adapters)?
                    .into_iter()
                    .map(|record| record.statement.id())
                    .collect();
                checked.insert(reverse_path.clone(), ids);
            }
            ensure!(
                !checked[&reverse_path].contains(&reverse_id),
                "reversed duplicate already exists: {reverse_id}"
            );
        }
    }
    Ok(records)
}

pub fn single_record(text: &str) -> Result<Record> {
    let mut records = parse(text)?;
    if records.len() != 1 {
        bail!("expected exactly one mapping");
    }
    Ok(records.remove(0))
}
