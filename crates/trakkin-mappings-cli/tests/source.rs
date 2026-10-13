use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Command};
use trakkin_mappings_source::storage::{Batch, Commit, Postgres, Update};
use trakkin_mappings_test_support::Database;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trakkin-mappings"));
    for variable in [
        "TRAKKIN_MAPPINGS_DATABASE_URL",
        "TRAKKIN_MAPPINGS_INGESTION_TMDB_TOKEN",
        "TRAKKIN_MAPPINGS_INGESTION_TVDB_API_KEY",
        "TRAKKIN_MAPPINGS_INGESTION_TVDB_PIN",
    ] {
        command.env_remove(variable);
    }
    command
}

#[test]
fn invalid_batches_domains_and_keys_fail_before_connecting() {
    for size in ["0", "1001"] {
        let output = command()
            .args(["source", "sync", "co.anilist", "--batch-size", size])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--batch-size"));
    }
    for source in ["co.anilist", "org.themoviedb", "com.thetvdb"] {
        let output = command()
            .args(["source", "sync", source, "--domain", "invalid"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Unsupported domain"));
    }
    for key in ["123", "episode:", "media:123"] {
        let output = command()
            .args(["source", "show", "org.themoviedb", key])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("qualified native key") || error.contains("Unsupported domain"),
            "{error}"
        );
    }
}

#[test]
fn sync_modes_parse_and_database_configuration_is_required() {
    for flags in [
        vec![],
        vec!["--full"],
        vec!["--restart"],
        vec!["--full", "--restart"],
    ] {
        let output = command()
            .args(["source", "sync", "co.anilist"])
            .args(flags)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("TRAKKIN_MAPPINGS_DATABASE_URL"));
    }
    let output = command()
        .args(["source", "status", "org.themoviedb"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("TRAKKIN_MAPPINGS_DATABASE_URL"));
}

#[test]
fn status_and_show_read_exact_records_and_tombstones_without_upstream_credentials() {
    let database = Database::start();
    let url = database.url.as_str();
    for (source, domain) in [
        ("co.anilist", "media"),
        ("org.themoviedb", "movie"),
        ("com.thetvdb", "movie"),
    ] {
        let live = format!("{domain}:{}", std::process::id());
        let gone = format!("{domain}:{}", std::process::id() + 1);
        let mut storage = Postgres::connect(url, source, &[domain]).unwrap();
        storage.begin().unwrap();
        storage
            .commit(Commit {
                state: &json!({"watermark":100}),
                batch: &Batch {
                    updates: BTreeMap::from([
                        (
                            live.clone(),
                            Update::record(&json!({"id":live}), json!({"address":"native"}))
                                .unwrap(),
                        ),
                        (
                            gone.clone(),
                            Update::deletion(&json!({"reason":"missing"}), json!({})).unwrap(),
                        ),
                    ]),
                },
                seen: &[],
                run_id: "cli-fixture",
            })
            .unwrap();
        let output = command()
            .env("TRAKKIN_MAPPINGS_DATABASE_URL", url)
            .args(["source", "status", source])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["source"], source);
        assert_eq!(status["scopes"][0]["initialized"], true);
        assert_eq!(status["scopes"][0]["watermark"], 100);
        for (key, deleted) in [(&live, false), (&gone, true)] {
            let output = command()
                .env("TRAKKIN_MAPPINGS_DATABASE_URL", url)
                .args(["source", "show", source, key])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["source"], source);
            assert_eq!(value["record"]["key"], *key);
            assert_eq!(value["record"]["deleted"], deleted);
            if !deleted {
                assert_eq!(value["record"]["metadata"]["address"], "native");
            }
        }
        let output = command()
            .env("TRAKKIN_MAPPINGS_DATABASE_URL", url)
            .args([
                "source",
                "show",
                source,
                &format!("{domain}:missing-cli-record"),
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("No record"));
    }
}
