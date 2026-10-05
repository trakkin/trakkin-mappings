use postgresql_archive::{
    VersionReq,
    blocking::{extract, get_archive},
    configuration::{theseus, zonky},
};
use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    let output = PathBuf::from(env::var("OUT_DIR")?);
    let version = VersionReq::parse("=18.2.0")?;
    let installation = output.join("postgresql");
    if !installation.join("bin").exists() {
        let (_, archive) = get_archive(zonky::URL, &version)?;
        extract(zonky::URL, &archive, &installation)?;
    }
    let clients = output.join("postgresql-clients");
    if !clients.join("bin").join("pg_dump").exists() {
        let (_, archive) = get_archive(theseus::URL, &version)?;
        extract(theseus::URL, &archive, &clients)?;
    }
    println!(
        "cargo:rustc-env=TRAKKIN_MAPPINGS_TEST_POSTGRESQL_DIR={}",
        installation.display()
    );
    println!(
        "cargo:rustc-env=TRAKKIN_MAPPINGS_TEST_POSTGRESQL_CLIENT_DIR={}",
        clients.display()
    );
    Ok(())
}
