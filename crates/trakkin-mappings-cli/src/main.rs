use anyhow::Result;
use clap::{Parser, Subcommand};

mod corpus;
mod source;

#[derive(Parser)]
#[command(
    version,
    about = "Synchronize native catalogs and maintain reviewed Git-backed mappings"
)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Maintain the canonical Trakkin mapping corpus.
    Corpus(corpus::Command),
    /// Synchronize and inspect native source catalogs.
    Source(source::Command),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Operation::Corpus(command) => command.run(),
        Operation::Source(command) => command.run(),
    }
}
