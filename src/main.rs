use clap::Parser as _;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing. Controlled via RUST_LOG env var, e.g.:
    //   RUST_LOG=agv=debug cargo run -- ls
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("agv=warn")),
        )
        .init();

    let cli = agv::cli::Cli::parse();
    if let Err(err) = agv::run(cli).await {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
    Ok(())
}
