use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "barry-dylan", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    Run {
        #[arg(long, default_value = "barry.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { config } => barry_dylan::app_runtime::run(&config).await,
    }
}
