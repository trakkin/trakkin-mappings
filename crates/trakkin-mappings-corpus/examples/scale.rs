use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use trakkin_mappings_corpus::{ADAPTERS, shard_path, single_record, statistics};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let output = PathBuf::from(arguments.next().context("usage: scale OUTPUT COUNT")?);
    let count: u64 = arguments.next().context("missing COUNT")?.parse()?;
    ensure!(
        count > 0 && count <= 50_000_000,
        "COUNT must be between 1 and 50000000"
    );
    ensure!(!output.exists(), "OUTPUT already exists");
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stage = tempfile::tempdir_in(parent)?;
    let buckets = tempfile::tempdir()?;
    let mut writers = (0..256)
        .map(|prefix| {
            File::create(buckets.path().join(format!("{prefix:02x}"))).map(BufWriter::new)
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let start = Instant::now();
    for index in 1..=count {
        let statement = format!("com.imdb://title/tt{index:07} <=> org.themoviedb://movie/{index}");
        let id = single_record(&statement)?.statement.id();
        let prefix = usize::from_str_radix(&id[..2], 16)?;
        writeln!(writers[prefix], "{id}\t{statement}")?;
    }
    for writer in &mut writers {
        writer.flush()?;
    }
    drop(writers);
    for prefix in 0..256 {
        let file = File::open(buckets.path().join(format!("{prefix:02x}")))?;
        let mut lines = BufReader::new(file)
            .lines()
            .collect::<std::io::Result<Vec<_>>>()?;
        lines.sort();
        let mut active: Option<(PathBuf, BufWriter<File>)> = None;
        for line in lines {
            let (id, statement) = line.split_once('\t').unwrap();
            let path = stage.path().join(shard_path(id)?);
            if active.as_ref().is_none_or(|(current, _)| current != &path) {
                if let Some((_, mut writer)) = active.take() {
                    writer.flush()?;
                }
                fs::create_dir_all(path.parent().unwrap())?;
                active = Some((path.clone(), BufWriter::new(File::create(path)?)));
            }
            let writer = &mut active.as_mut().unwrap().1;
            writeln!(
                writer,
                "#@reason manual-verification\n#@note Synthetic load-test record only. Identifiers do not assert real media correspondence. Never merge this generated corpus.\n{statement}"
            )?;
        }
        if let Some((_, mut writer)) = active {
            writer.flush()?;
        }
    }
    let adapters = stage.path().join(ADAPTERS);
    fs::create_dir_all(adapters.parent().unwrap())?;
    fs::write(
        adapters,
        include_bytes!("../../../mappings/v1/adapters.json"),
    )?;
    fs::rename(stage.path(), &output)?;
    eprintln!(
        "generated {count} records in {:.2}s",
        start.elapsed().as_secs_f64()
    );
    println!("{}", serde_json::to_string_pretty(&statistics(&output)?)?);
    Ok(())
}
