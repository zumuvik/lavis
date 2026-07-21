use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize structured logging: {error}"))?;

    lavis::run().await
}
