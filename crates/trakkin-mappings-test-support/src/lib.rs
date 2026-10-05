use postgresql_embedded::{SettingsBuilder, blocking::PostgreSQL};
use std::{fs, io::Write, path::Path, process::Command, time::Duration};
use tempfile::TempDir;
use url::Url;

pub struct Database {
    postgresql: PostgreSQL,
    pub url: Url,
    _directory: TempDir,
}

impl Database {
    pub fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = SettingsBuilder::new()
            .installation_dir(env!("TRAKKIN_MAPPINGS_TEST_POSTGRESQL_DIR"))
            .trust_installation_dir(true)
            .data_dir(directory.path().join("data"))
            .password_file(directory.path().join(".pgpass"))
            .host("127.0.0.1")
            .port(0)
            .username("postgres")
            .password("test")
            .timeout(Some(Duration::from_secs(30)));
        #[cfg(unix)]
        {
            let socket = directory.path().join("socket");
            fs::create_dir_all(&socket).unwrap();
            settings = settings.socket_dir(socket);
        }
        let mut postgresql = PostgreSQL::new(settings.build());
        postgresql.setup().unwrap();
        postgresql.start().unwrap();
        postgresql.create_database("test").unwrap();
        let url = Url::parse(&postgresql.settings().url("test")).unwrap();
        Self {
            postgresql,
            url,
            _directory: directory,
        }
    }

    pub fn backup(&self) -> Vec<u8> {
        let output = Command::new(
            Path::new(env!("TRAKKIN_MAPPINGS_TEST_POSTGRESQL_CLIENT_DIR"))
                .join("bin")
                .join("pg_dump"),
        )
        .arg("--format=custom")
        .arg("--dbname")
        .arg(self.url.as_str())
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "pg_dump failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    pub fn restore(&self, archive: &[u8]) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(archive).unwrap();
        let output = Command::new(
            Path::new(env!("TRAKKIN_MAPPINGS_TEST_POSTGRESQL_CLIENT_DIR"))
                .join("bin")
                .join("pg_restore"),
        )
        .args([
            "--exit-on-error",
            "--no-owner",
            "--no-privileges",
            "--dbname",
        ])
        .arg(self.url.as_str())
        .arg(file.path())
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "pg_restore failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.postgresql.stop();
    }
}
