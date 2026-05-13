use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing_subscriber::{prelude::*, EnvFilter};

pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,barry_bot=debug"));
    let fmt = tracing_subscriber::fmt::layer().json();
    let _ = tracing_subscriber::registry().with(filter).with(fmt).try_init();
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
