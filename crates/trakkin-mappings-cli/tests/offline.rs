use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn formatting_preserves_mapping_semantics_without_a_database() {
    let input = "[b://1,b://2]  =>  a://3 @2\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_trakkin-mappings"))
        .args(["corpus", "fmt"])
        .env_remove("TRAKKIN_MAPPINGS_DATABASE_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        trakkin_mappings_language::parse_record(&text)
            .unwrap()
            .statement,
        trakkin_mappings_language::parse_record(input)
            .unwrap()
            .statement
    );
}
