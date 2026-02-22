use clap::Parser as _;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = agv::cli::Cli::parse();

    // Determine tracing level from CLI flags. The RUST_LOG env var takes
    // precedence if set, otherwise we pick a default based on --verbose / --quiet.
    let default_filter = if cli.verbose {
        "agv=info"
    } else if cli.quiet {
        "agv=error"
    } else {
        "agv=warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .init();

    if let Err(err) = agv::run(cli).await {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
    Ok(())
}
