use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("iscsid=info")),
        )
        .init();

    info!("iscsid v{}", env!("CARGO_PKG_VERSION"));
    info!("Daemon not yet implemented — see Phase 5");

    Ok(())
}
