use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

const MAPPING: &str = "com.imdb://title/tt0133093 <=> org.themoviedb://movie/603";

fn run(root: &Path, arguments: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_trakkin-mappings"))
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

fn success(root: &Path, arguments: &[&str], input: &str) -> String {
    let result = run(root, arguments, input);
    assert!(
        result.status.success(),
        "{:?}: {}",
        arguments,
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
    let canonical = success(root.path(), &["fmt", "--corpus"], &input);
    assert_eq!(
        canonical,
        format!("#@source a\n#@source z\n#@note verified\n{MAPPING}\n")
    );
    success(
        root.path(),
        &["fmt", "--corpus", "--check", "-"],
        &canonical,
    );
    assert!(
        !run(root.path(), &["fmt", "--corpus", "--check"], &input)
            .status
            .success()
    );
    assert!(success(root.path(), &["insert"], &input).contains("1 record(s) changed"));
    assert!(success(root.path(), &["insert"], &canonical).contains("0 record(s) changed"));
    let location: serde_json::Value =
        serde_json::from_str(&success(root.path(), &["locate", MAPPING], "")).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join(location["shard"].as_str().unwrap())).unwrap(),
        canonical
    );
    success(root.path(), &["validate", "--indexed"], "");
    let report: serde_json::Value =
        serde_json::from_str(&success(root.path(), &["index"], "")).unwrap();
    assert_eq!(report["rebuilt_shards"], 0);
    let matches: serde_json::Value = serde_json::from_str(&success(
        root.path(),
        &["query", "org.themoviedb://movie/603"],
        "",
    ))
    .unwrap();
    assert_eq!(matches[0]["record"], canonical);
    let id = location["id"].as_str().unwrap();
    assert!(success(root.path(), &["remove", id], "").contains("removed: true"));
    assert!(success(root.path(), &["remove", id], "").contains("removed: false"));
    success(root.path(), &["index"], "");
    assert_eq!(
        success(root.path(), &["query", "org.themoviedb://movie/603"], "").trim(),
        "[]"
    );
}

#[test]
fn formatting_does_not_restrict_general_language_metadata() {
    let root = tempfile::tempdir().unwrap();
    let input = format!("# human comment\n#@custom value\n{MAPPING}");
    assert_eq!(success(root.path(), &["fmt"], &input), format!("{input}\n"));
    assert!(
        !run(root.path(), &["fmt", "--corpus"], &input)
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
        success(root.path(), &["insert"], &input),
        "2 record(s) changed\n"
    );
    assert!(success(root.path(), &["validate"], "").contains("validated 2 records"));
}
