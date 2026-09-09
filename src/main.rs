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
    /// Run the webhook server and job dispatcher.
    Run {
        #[arg(long, default_value = "barry.toml")]
        config: PathBuf,
    },
    /// Produce a review by running it on the rack, with no GitHub access.
    ///
    /// Submits a systemslab experiment that creates an ephemeral GPU VM, serves
    /// a model in it, runs `review-offline` inside, and returns the review.
    /// The same inputs and the same output as `review-offline` — only where the
    /// model runs differs.
    ReviewRack {
        #[arg(long)]
        config: PathBuf,
        /// JSON array of ChangedFile.
        #[arg(long)]
        files: PathBuf,
        /// Where to write the review JSON.
        #[arg(long)]
        out: PathBuf,
        /// Experiment name, to make it findable in systemslab.
        #[arg(long, default_value = "barry review")]
        name: String,
    },
    /// Produce a review from a changed-file set, with no GitHub access.
    ///
    /// Reads a JSON array of ChangedFile, runs the local-model half of the
    /// review pipeline, and writes a UnifiedReview as JSON.
    ReviewOffline {
        #[arg(long)]
        config: PathBuf,
        /// JSON array of ChangedFile.
        #[arg(long)]
        files: PathBuf,
        /// Where to write the review JSON.
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { config } => barry_dylan::app_runtime::run(&config).await,
        Cmd::ReviewOffline { config, files, out } => review_offline(&config, &files, &out).await,
        Cmd::ReviewRack {
            config,
            files,
            out,
            name,
        } => review_rack(&config, &files, &out, &name).await,
    }
}

/// Read a JSON array of ChangedFile, or explain which file was wrong.
fn read_changed_files(
    files: &std::path::Path,
) -> anyhow::Result<Vec<barry_dylan::github::pr::ChangedFile>> {
    let text = std::fs::read_to_string(files)
        .map_err(|e| anyhow::anyhow!("reading changed files {}: {e}", files.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing changed files {}: {e}", files.display()))
}

async fn review_rack(
    config: &std::path::Path,
    files: &std::path::Path,
    out: &std::path::Path,
    name: &str,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(config)
        .map_err(|e| anyhow::anyhow!("reading rack config {}: {e}", config.display()))?;
    let cfg: barry_dylan::rack::RackConfig = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing rack config {}: {e}", config.display()))?;

    let changed = read_changed_files(files)?;

    // No global timeout here: the config's job_timeout_secs already bounds the
    // wait, and it is the one that knows how long a model pull takes.
    let http = reqwest::Client::new();
    let review = barry_dylan::rack::review(&cfg, &http, &changed, name).await?;

    let json = serde_json::to_string_pretty(&review)?;
    std::fs::write(out, json)
        .map_err(|e| anyhow::anyhow!("writing review {}: {e}", out.display()))?;
    Ok(())
}

async fn review_offline(
    config: &std::path::Path,
    files: &std::path::Path,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    let cfg = barry_dylan::offline::config::load(config)?;

    let files_text = std::fs::read_to_string(files)
        .map_err(|e| anyhow::anyhow!("reading changed files {}: {e}", files.display()))?;
    let changed: Vec<barry_dylan::github::pr::ChangedFile> = serde_json::from_str(&files_text)
        .map_err(|e| anyhow::anyhow!("parsing changed files {}: {e}", files.display()))?;

    let review = barry_dylan::offline::run(&cfg, &changed).await?;

    let json = serde_json::to_string_pretty(&review)?;
    std::fs::write(out, json)
        .map_err(|e| anyhow::anyhow!("writing review {}: {e}", out.display()))?;

    eprintln!("wrote review to {}", out.display());
    Ok(())
}
