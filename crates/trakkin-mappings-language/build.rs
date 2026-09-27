use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let grammar_dir = manifest_dir.join("../../grammar");
    let generation = antlr_rust_codegen::Builder::new()
        .grammar(grammar_dir.join("Trakkin.g4"))
        .library_directory(&grammar_dir)
        .out_dir(env::var_os("OUT_DIR").unwrap())
        .generate()?;
    generation.emit_rerun_if_changed();
    Ok(())
}
