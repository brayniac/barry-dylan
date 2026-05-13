use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "barry-bot", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand, Debug)]
enum Cmd {
    /// Run the bot service.
    Run {
        #[arg(long, default_value = "barry.toml")]
        config: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { config } => {
            println!("would load config from {}", config.display());
            Ok(())
        }
    }
}
