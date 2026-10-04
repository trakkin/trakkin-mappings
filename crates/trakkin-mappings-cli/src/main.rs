use anyhow::Result;
use clap::{Parser, Subcommand};

mod corpus;
#[derive(Parser)]
#[command(
    version,
    about = "Maintain mapping corpora and ingest source-native provider catalogues"
)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    /// Maintain the canonical Trakkin mapping corpus.
    Corpus(corpus::Command),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Operation::Corpus(command) => command.run(),
    }
}
