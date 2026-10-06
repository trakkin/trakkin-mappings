use std::process::Command;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trakkin-mappings"));
    command.env_remove("TRAKKIN_MAPPINGS_INGESTION_WAREHOUSE");
    command
}

#[test]
fn ingestion_commands_expose_help_without_configuration() {
    for operation in [
        "bootstrap",
        "sync",
        "reconcile",
        "maintain",
        "validate",
        "inspect",
    ] {
        let output = command()
            .args(["ingestion", operation, "--help"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("--warehouse"));
        assert!(help.contains("--provider"));
        assert!(help.contains("--domain"));
        assert!(help.contains("[default: 1000]"));
    }
}

#[test]
fn invalid_batch_sizes_fail_before_configuration_or_storage() {
    for size in ["0", "1001"] {
        let output = command()
            .args(["ingestion", "--batch-size", size, "bootstrap"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--batch-size"));
        assert!(!stderr.contains("Set --warehouse"));
    }
}

#[test]
fn canonical_provider_ids_and_domain_validation() {
    for source in ["co.anilist", "org.themoviedb", "com.thetvdb"] {
        let output = command()
            .args([
                "ingestion",
                "--warehouse",
                "/tmp/unused",
                "--provider",
                source,
                "--domain",
                "invalid",
                "validate",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Unsupported domain"));
    }
}

#[test]
fn warehouse_flag_overrides_environment() {
    let output = command()
        .env("TRAKKIN_MAPPINGS_INGESTION_WAREHOUSE", "/tmp/unused")
        .args([
            "ingestion",
            "--provider",
            "com.thetvdb",
            "--domain",
            "series",
            "--warehouse",
            "",
            "validate",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("Warehouse must not be empty"));
    let output = command()
        .env("TRAKKIN_MAPPINGS_INGESTION_WAREHOUSE", "/tmp/unused")
        .args([
            "ingestion",
            "--provider",
            "com.thetvdb",
            "--domain",
            "invalid",
            "validate",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unsupported domain"));
}

#[test]
fn writer_lock_defaults_to_selected_bridge_directory() {
    let directory = tempfile::tempdir().unwrap();
    let bridge = directory.path().join("bridge");
    let output = command()
        .args([
            "ingestion",
            "--warehouse",
            directory.path().to_str().unwrap(),
            "--provider",
            "co.anilist",
            "--bridge",
            bridge.to_str().unwrap(),
            "validate",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Build the Java bridge"));
    assert!(bridge.join("writer.lock").is_file());
}

#[test]
fn warehouse_configuration_is_required() {
    let output = command().args(["ingestion", "validate"]).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Set --warehouse"));
    assert!(!stderr.contains('\u{1b}'));
}
