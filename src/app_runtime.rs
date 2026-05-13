use crate::config::Config;
use crate::dispatcher::run::{GhFactory, JobDeps, Pipeline};
use crate::github::app::AppCreds;
use crate::github::client::GitHub;
use crate::storage::Store;
use crate::webhook::server::AppState;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

pub struct AppGhFactory {
    pub creds: Arc<AppCreds>,
    pub http: reqwest::Client,
    pub store: Store,
}

#[async_trait]
impl GhFactory for AppGhFactory {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        let token = crate::github::app::get_or_mint(
            &self.store, &self.http, &self.creds, installation_id, now,
        ).await?;
        Ok(Arc::new(GitHub::new(self.http.clone(), token)))
    }
}

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    crate::telemetry::init_tracing();
    let cfg = Arc::new(Config::load(config_path)?);

    // Read secrets.
    let webhook_secret = std::env::var(&cfg.github.webhook_secret_env)
        .map_err(|_| anyhow::anyhow!("env var {} not set", cfg.github.webhook_secret_env))?;
    crate::github::app::ensure_key_mode_strict(&cfg.github.private_key_path)?;
    let creds = Arc::new(AppCreds::load(cfg.github.app_id, &cfg.github.private_key_path)?);

    let store = Store::open(&cfg.storage.sqlite_path).await?;
    let metrics = crate::telemetry::install_metrics();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let gh_factory: Arc<dyn GhFactory> = Arc::new(AppGhFactory {
        creds: creds.clone(),
        http: http.clone(),
        store: store.clone(),
    });

    let pipeline = Arc::new(build_pipeline(&cfg)?);
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg.clone(),
        pipeline: pipeline.clone(),
        gh_factory: gh_factory.clone(),
    });

    // Workers.
    for _ in 0..cfg.dispatcher.worker_count {
        let deps = deps.clone();
        let lease = cfg.dispatcher.job_timeout_secs as i64;
        tokio::spawn(async move { crate::dispatcher::worker::run_worker(deps, lease).await });
    }

    // HTTP server.
    let app_state = AppState {
        store: store.clone(),
        webhook_secret: Arc::new(webhook_secret.into_bytes()),
        metrics,
        debounce_secs: cfg.dispatcher.debounce_secs,
    };
    let router = crate::webhook::server::router(app_state);
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen).await?;
    tracing::info!(addr = %cfg.server.listen, "barry-dylan listening");

    let server = axum::serve(listener, router);

    let server_task = tokio::spawn(async move { server.await });
    let mut sighup = signal(SignalKind::hangup())?;
    loop {
        tokio::select! {
            _ = sighup.recv() => {
                tracing::info!("SIGHUP — reloading config");
                match Config::load(config_path) {
                    Ok(new_cfg) => tracing::info!(workers = new_cfg.dispatcher.worker_count, "reloaded"),
                    Err(e) => tracing::error!(?e, "reload failed; keeping previous config"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c; shutting down");
                break;
            }
        }
    }
    server_task.abort();
    Ok(())
}

fn build_pipeline(cfg: &Config) -> anyhow::Result<Pipeline> {
    let mut p = Pipeline::hygiene_only();

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            cfg.llm.get("default").map(|d| d.request_timeout_secs).unwrap_or(300)))
        .build()?;

    let profile = cfg.llm.get("default")
        .ok_or_else(|| anyhow::anyhow!("missing [llm.default]"))?;
    let client = crate::llm::factory::build(profile, http)?;

    p.checkers.push(Arc::new(crate::checker::llm_review::LlmReviewChecker {
        client,
        max_tokens: profile.max_tokens,
    }));
    Ok(p)
}
