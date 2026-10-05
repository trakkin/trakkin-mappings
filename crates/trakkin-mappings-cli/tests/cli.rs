use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trakkin-mappings"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("TRAKKIN_MAPPINGS_")) {
        command.env_remove(key);
    }
    command
}

fn run(arguments: &[&str]) -> Output {
    command().args(arguments).output().unwrap()
}

#[test]
fn exposes_corpus_namespace() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("corpus"));
    assert!(help.contains("ingestion"));
    assert!(!help.contains("--root"));
    for command in [
        "fmt", "locate", "insert", "remove", "validate", "index", "query", "release", "stats",
    ] {
        assert!(run(&["corpus", command, "--help"]).status.success());
    }
    assert!(!run(&["check", "org.themoviedb"]).status.success());
}

const MAPPING: &str = "com.imdb://title/tt0133093 <=> org.themoviedb://movie/603";

fn corpus_run(root: &Path, arguments: &[&str], input: &str) -> Output {
    let mut child = command()
        .arg("corpus")
        .arg("--root")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn corpus_success(root: &Path, arguments: &[&str], input: &str) -> String {
    let result = corpus_run(root, arguments, input);
    assert!(
        result.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

fn install_adapters(root: &Path) {
    let path = root.join(trakkin_mappings_corpus::ADAPTERS);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, include_bytes!("../../../mappings/v1/adapters.json")).unwrap();
}

#[test]
fn maintainer_workflow_from_stdin_to_query_and_removal() {
    let root = tempfile::tempdir().unwrap();
    install_adapters(root.path());
    let input = format!("#@note  verified  \n#@source z\n#@source a\n#@source z\n{MAPPING}");
    let canonical = corpus_success(root.path(), &["fmt", "--corpus"], &input);
    assert_eq!(
        canonical,
        format!("#@source a\n#@source z\n#@note verified\n{MAPPING}\n")
    );
    corpus_success(
        root.path(),
        &["fmt", "--corpus", "--check", "-"],
        &canonical,
    );
    assert!(
        !corpus_run(root.path(), &["fmt", "--corpus", "--check"], &input)
            .status
            .success()
    );
    assert!(corpus_success(root.path(), &["insert"], &input).contains("1 record(s) changed"));
    assert!(corpus_success(root.path(), &["insert"], &canonical).contains("0 record(s) changed"));
    let location: serde_json::Value =
        serde_json::from_str(&corpus_success(root.path(), &["locate", MAPPING], "")).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join(location["shard"].as_str().unwrap())).unwrap(),
        canonical
    );
    corpus_success(root.path(), &["validate", "--indexed"], "");
    let report: serde_json::Value =
        serde_json::from_str(&corpus_success(root.path(), &["index"], "")).unwrap();
    assert_eq!(report["rebuilt_shards"], 0);
    let matches: serde_json::Value = serde_json::from_str(&corpus_success(
        root.path(),
        &["query", "org.themoviedb://movie/603"],
        "",
    ))
    .unwrap();
    assert_eq!(matches[0]["record"], canonical);
    let id = location["id"].as_str().unwrap();
    assert!(corpus_success(root.path(), &["remove", id], "").contains("removed: true"));
    assert!(corpus_success(root.path(), &["remove", id], "").contains("removed: false"));
    corpus_success(root.path(), &["index"], "");
    assert_eq!(
        corpus_success(root.path(), &["query", "org.themoviedb://movie/603"], "").trim(),
        "[]"
    );
}

#[test]
fn formatting_does_not_restrict_general_language_metadata() {
    let root = tempfile::tempdir().unwrap();
    let input = format!("# human comment\n#@custom value\n{MAPPING}");
    assert_eq!(
        corpus_success(root.path(), &["fmt"], &input),
        format!("{input}\n")
    );
    assert!(
        !corpus_run(root.path(), &["fmt", "--corpus"], &input)
            .status
            .success()
    );
}

#[test]
fn insert_reads_multiple_records_until_stdin_eof() {
    let root = tempfile::tempdir().unwrap();
    install_adapters(root.path());
    let input = format!("{MAPPING}\ncom.imdb://title/tt0234215 <=> org.themoviedb://movie/604\n");
    assert_eq!(
        corpus_success(root.path(), &["insert"], &input),
        "2 record(s) changed\n"
    );
    assert!(corpus_success(root.path(), &["validate"], "").contains("validated 2 records"));
}

#[test]
fn corpus_options_are_scoped_and_work_after_leaf_commands() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("custom-adapters.json");
    fs::write(
        &registry,
        include_bytes!("../../../mappings/v1/adapters.json"),
    )
    .unwrap();
    corpus_success(
        root.path(),
        &[
            "insert",
            "--root",
            root.path().to_str().unwrap(),
            "--adapters",
            registry.to_str().unwrap(),
        ],
        MAPPING,
    );
    let output = run(&["corpus", "--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--root"));
    assert!(help.contains("--adapters"));
    assert!(!run(&["--root", ".", "corpus", "stats"]).status.success());
}
