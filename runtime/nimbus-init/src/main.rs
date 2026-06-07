use std::process::ExitCode;

use nimbus_init::run;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "nimbus-init failed");
            // Try to surface the error to the host over vsock.
            // Best effort: we may not have a working connection.
            ExitCode::FAILURE
        }
    }
}
