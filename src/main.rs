use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Installed before app startup so early runtime warnings are forwarded.
    lavis::log_forwarder::install_bridge(64);

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().compact())
        .with(filter)
        .with(lavis::log_forwarder::bridge_layer())
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize structured logging: {error}"))?;

    match lavis::run().await {
        Ok(()) => Ok(()),
        Err(error) if lavis::requires_manual_recovery(&error) => {
            eprintln!("{error:#}");
            std::process::exit(78);
        }
        Err(error) => Err(error),
    }
}
