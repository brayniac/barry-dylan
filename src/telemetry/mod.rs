use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::{EnvFilter, prelude::*};

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Human-readable text to stderr for operators reading in the terminal.
    let text_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    // Structured JSON to stdout for log aggregation / monitoring.
    let json_layer = tracing_subscriber::fmt::layer().json();

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(text_layer)
        .with(json_layer)
        .try_init();
}

pub fn install_metrics() -> PrometheusHandle {
    PrometheusBuilder::new()
        .install_recorder()
        .expect("install Prometheus recorder")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn install_metrics_returns_handle() {
        let h = install_metrics();
        let _ = h.render();
    }
}
