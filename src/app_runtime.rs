use crate::checker::multi_review::identity::Identity;
use crate::config::Config;
use crate::dispatcher::run::{GhFactory, GhFactoryError, JobDeps, MultiGhFactory, Pipeline};
use crate::github::app::AppCreds;
use crate::github::client::GitHub;
use crate::storage::Store;
use crate::telemetry::status::StatusTracker;
use crate::webhook::server::AppState;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

pub struct AppGhFactory {
    pub barry: Arc<AppCreds>,
    pub other_barry: Arc<AppCreds>,
    pub other_other_barry: Arc<AppCreds>,
    pub http: reqwest::Client,
    pub store: Store,
    pub gh_api_base: Option<String>,
}

impl AppGhFactory {
    fn creds_for(&self, identity: Identity) -> &Arc<AppCreds> {
        match identity {
            Identity::Barry => &self.barry,
            Identity::OtherBarry => &self.other_barry,
            Identity::OtherOtherBarry => &self.other_other_barry,
        }
    }
}

#[async_trait]
impl GhFactory for AppGhFactory {
    async fn for_installation(&self, installation_id: i64) -> anyhow::Result<Arc<GitHub>> {
        // Used by code paths that already have Barry's installation_id in hand
        // (e.g., dispatcher leasing a job). Identity-based callers should use
        // for_identity() so OB and OOB get resolved correctly.
        let now = crate::util::now_ts();
        let base = self
            .gh_api_base
            .as_deref()
            .unwrap_or(crate::github::app::GITHUB_API_BASE);
        let token = crate::github::app::get_or_mint_for(
            &self.store,
            &self.http,
            &self.barry,
            Identity::Barry,
            installation_id,
            now,
            base,
        )
        .await?;
        Ok(Arc::new(GitHub::new(self.http.clone(), token)))
    }
}

#[async_trait]
impl MultiGhFactory for AppGhFactory {
    async fn for_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<Arc<GitHub>, GhFactoryError> {
        let now = crate::util::now_ts();
        let base = self
            .gh_api_base
            .as_deref()
            .unwrap_or(crate::github::app::GITHUB_API_BASE);
        let creds = self.creds_for(identity);
        let installation_id = self
            .resolve_installation(identity, owner, repo, now)
            .await?;
        match crate::github::app::get_or_mint_for(
            &self.store,
            &self.http,
            creds,
            identity,
            installation_id,
            now,
            base,
        )
        .await
        {
            Ok(token) => Ok(Arc::new(GitHub::new(self.http.clone(), token))),
            Err(e) if is_unauthorized(&e) => {
                // Stale positive cache: App was uninstalled. Invalidate, retry once.
                self.store
                    .invalidate_installation(identity.slug(), owner)
                    .await
                    .map_err(GhFactoryError::Other)?;
                let installation_id = self
                    .resolve_installation(identity, owner, repo, now)
                    .await?;
                let token = crate::github::app::get_or_mint_for(
                    &self.store,
                    &self.http,
                    creds,
                    identity,
                    installation_id,
                    now,
                    base,
                )
                .await
                .map_err(GhFactoryError::Other)?;
                Ok(Arc::new(GitHub::new(self.http.clone(), token)))
            }
            Err(e) => Err(GhFactoryError::Other(e)),
        }
    }

    async fn preflight_identity(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
    ) -> Result<(), GhFactoryError> {
        let now = crate::util::now_ts();
        let _ = self
            .resolve_installation(identity, owner, repo, now)
            .await?;
        Ok(())
    }
}

impl AppGhFactory {
    /// Resolve (identity, owner) → installation_id using the cache, falling
    /// back to a GitHub API lookup on miss. On 404, writes a negative cache
    /// entry and returns `NotInstalled`. WARN + metric are emitted exactly
    /// once per (identity, owner) thanks to the cache short-circuit.
    async fn resolve_installation(
        &self,
        identity: Identity,
        owner: &str,
        repo: &str,
        now: i64,
    ) -> Result<i64, GhFactoryError> {
        // Cache check.
        match self
            .store
            .get_installation(identity.slug(), owner, now)
            .await
            .map_err(GhFactoryError::Other)?
        {
            Some(crate::storage::CachedInstallation::Cached { installation_id }) => {
                return Ok(installation_id);
            }
            Some(crate::storage::CachedInstallation::NotInstalled) => {
                return Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                });
            }
            None => {}
        }
        // Miss → GitHub lookup.
        let base = self
            .gh_api_base
            .as_deref()
            .unwrap_or(crate::github::app::GITHUB_API_BASE);
        let creds = self.creds_for(identity);
        let resolved = crate::github::app::resolve_installation_id_for_repo(
            &self.http, creds, owner, repo, base,
        )
        .await
        .map_err(GhFactoryError::Other)?;
        match resolved {
            Some(id) => {
                self.store
                    .put_installation(identity.slug(), owner, Some(id), now)
                    .await
                    .map_err(GhFactoryError::Other)?;
                Ok(id)
            }
            None => {
                self.store
                    .put_installation(identity.slug(), owner, None, now)
                    .await
                    .map_err(GhFactoryError::Other)?;
                tracing::warn!(
                    identity = %identity.slug(),
                    owner = %owner,
                    repo = %repo,
                    "App not installed on owner; subsequent calls will use Barry alone"
                );
                metrics::counter!(
                    "barry_multi_review_identity_missing_total",
                    "identity" => identity.slug().to_string(),
                    "owner" => owner.to_string(),
                )
                .increment(1);
                Err(GhFactoryError::NotInstalled {
                    identity,
                    owner: owner.to_string(),
                    repo: repo.to_string(),
                })
            }
        }
    }
}

pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    crate::telemetry::init_tracing();
    let cfg = Arc::new(Config::load(config_path)?);

    // Read secrets.
    let webhook_env = cfg
        .github
        .barry
        .webhook_secret_env
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("[github.barry].webhook_secret_env required"))?;
    let webhook_secret = std::env::var(webhook_env)
        .map_err(|_| anyhow::anyhow!("env var {} not set", webhook_env))?;

    crate::github::app::ensure_key_mode_strict(&cfg.github.barry.private_key_path)?;
    crate::github::app::ensure_key_mode_strict(&cfg.github.other_barry.private_key_path)?;
    crate::github::app::ensure_key_mode_strict(&cfg.github.other_other_barry.private_key_path)?;

    let barry = Arc::new(AppCreds::load(
        cfg.github.barry.app_id,
        &cfg.github.barry.private_key_path,
    )?);
    let ob = Arc::new(AppCreds::load(
        cfg.github.other_barry.app_id,
        &cfg.github.other_barry.private_key_path,
    )?);
    let oob = Arc::new(AppCreds::load(
        cfg.github.other_other_barry.app_id,
        &cfg.github.other_other_barry.private_key_path,
    )?);

    let store = Store::open(&cfg.storage.sqlite_path).await?;
    let metrics = crate::telemetry::install_metrics();
    let status_tracker = Arc::new(StatusTracker::new());
    crate::telemetry::spawn_status_ticker(status_tracker.clone());
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let gh_factory: Arc<dyn MultiGhFactory> = Arc::new(AppGhFactory {
        barry,
        other_barry: ob,
        other_other_barry: oob,
        store: store.clone(),
        http: http.clone(),
        gh_api_base: None,
    });

    let clients = Arc::new(crate::checker::multi_review::clients::build(&cfg)?);
    let overrides = crate::checker::multi_review::persona::overrides_from_config(&cfg.personas);
    let personas = Arc::new(crate::checker::multi_review::persona::resolve(&overrides)?);
    let rack = cfg.rack.clone().map(Arc::new);
    if let Some(r) = &rack {
        tracing::info!(
            systemslab = %r.systemslab,
            placement = ?r.placement,
            reviewers = r.reviewers.len(),
            "reviews will run on the rack"
        );
    }
    let pipeline = Arc::new(build_pipeline_with(
        clients.clone(),
        personas.clone(),
        gh_factory.clone(),
        status_tracker.clone(),
        rack,
    ));
    let cancel_registry = crate::dispatcher::cancel::CancelRegistry::new();
    let deps = Arc::new(JobDeps {
        store: store.clone(),
        config: cfg.clone(),
        pipeline: pipeline.clone(),
        gh_factory: gh_factory.clone(),
        clients: Some(clients),
        personas: Some(personas),
        status_tracker: status_tracker.clone(),
        cancel_registry,
    });

    // Shared cancellation token for graceful shutdown.
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    // Workers.
    let mut worker_handles = Vec::with_capacity(cfg.dispatcher.worker_count);
    for _ in 0..cfg.dispatcher.worker_count {
        let deps = deps.clone();
        let lease = cfg.dispatcher.job_timeout_secs as i64;
        let shutdown = shutdown_clone.clone();
        let handle = tokio::spawn(async move {
            crate::dispatcher::worker::run_worker(deps, lease, shutdown).await
        });
        worker_handles.push(handle);
    }

    // The relay, when this barry is somewhere GitHub cannot reach. Started
    // before the server binds only because it needs the secret before it is
    // moved into the router; it tolerates a target that is not up yet, since
    // its first delivery is however long GitHub takes to send one.
    let webhook_secret = Arc::new(webhook_secret.into_bytes());
    let relay_status = cfg.relay.as_ref().map(|relay_cfg| {
        let status = Arc::new(crate::relay::Status::default());
        let target = crate::relay::target_url(&cfg.server.listen);
        tokio::spawn(crate::relay::run(
            relay_cfg.clone(),
            target,
            webhook_secret.clone(),
            status.clone(),
            shutdown_clone.clone(),
        ));
        status
    });

    // HTTP server.
    let app_state = AppState {
        store: store.clone(),
        webhook_secret,
        metrics,
        debounce_secs: cfg.dispatcher.debounce_secs,
        relay: relay_status,
    };
    let router = crate::webhook::server::router(app_state);
    let listener = tokio::net::TcpListener::bind(&cfg.server.listen).await?;
    tracing::info!(addr = %cfg.server.listen, "barry-dylan listening");

    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown_clone.cancelled().await;
    });

    // SIGHUP — reload config. SIGTERM — graceful shutdown.
    let mut sighup = signal(SignalKind::hangup())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let server_task = tokio::spawn(async move { server.await });

    loop {
        tokio::select! {
            _ = sighup.recv() => {
                tracing::info!("SIGHUP — reloading config");
                match Config::load(config_path) {
                    Ok(new_cfg) => tracing::info!(workers = new_cfg.dispatcher.worker_count, "reloaded"),
                    Err(e) => tracing::error!(?e, "reload failed; keeping previous config"),
                }
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received — shutting down gracefully");
                shutdown.cancel();
                break;
            }
        }
    }

    // Server drains in-flight requests via with_graceful_shutdown.
    let _ = server_task.await;

    // Workers finish their current job and exit the lease loop.
    for handle in worker_handles {
        let _ = handle.await;
    }

    tracing::info!("shutdown complete");
    Ok(())
}

fn build_pipeline_with(
    clients: Arc<crate::checker::multi_review::clients::IdentityClients>,
    personas: Arc<Vec<crate::checker::multi_review::persona::Persona>>,
    gh_factory: Arc<dyn MultiGhFactory>,
    status_tracker: Arc<StatusTracker>,
    rack: Option<Arc<crate::rack::RackConfig>>,
) -> Pipeline {
    let mut p = Pipeline::hygiene_only();
    p.checkers
        .push(Arc::new(crate::checker::multi_review::MultiReviewChecker {
            clients,
            personas,
            gh_factory,
            status_tracker,
            rack,
            http: reqwest::Client::new(),
        }));
    p
}

fn is_unauthorized(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|re| re.status() == Some(reqwest::StatusCode::UNAUTHORIZED))
}

#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::checker::multi_review::identity::Identity;
    use crate::storage::Store;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_creds() -> Arc<crate::github::app::AppCreds> {
        // Use the test key fixture shared with github::app tests.
        Arc::new(crate::github::app::AppCreds::from_pem_bytes(
            12345,
            include_bytes!("../tests/fixtures/test_app_key.pem").to_vec(),
        ))
    }

    async fn factory_with(server_uri: String) -> (AppGhFactory, Store) {
        let store = Store::in_memory().await.unwrap();
        let http = reqwest::Client::new();
        let f = AppGhFactory {
            barry: test_creds(),
            other_barry: test_creds(),
            other_other_barry: test_creds(),
            store: store.clone(),
            http,
            gh_api_base: Some(server_uri),
        };
        (f, store)
    }

    #[tokio::test]
    async fn cache_miss_resolves_and_caches_positive() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 99})))
            .expect(1)
            .mount(&server)
            .await;
        let (f, store) = factory_with(server.uri()).await;
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
        // Second call should hit cache, not the mock.
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
        let v = store
            .get_installation("other_barry", "acme", crate::util::now_ts())
            .await
            .unwrap();
        assert_eq!(
            v,
            Some(crate::storage::CachedInstallation::Cached {
                installation_id: 99
            })
        );
    }

    #[tokio::test]
    async fn cache_miss_404_writes_negative_and_returns_not_installed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let (f, store) = factory_with(server.uri()).await;
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
        // Second call hits negative cache; no additional HTTP traffic.
        let err = f
            .preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap_err();
        assert!(matches!(err, GhFactoryError::NotInstalled { .. }));
        let v = store
            .get_installation("other_barry", "acme", crate::util::now_ts())
            .await
            .unwrap();
        assert_eq!(v, Some(crate::storage::CachedInstallation::NotInstalled));
    }

    #[tokio::test]
    async fn positive_cache_hit_skips_http() {
        let server = MockServer::start().await;
        // No mounted mocks — any HTTP call here would fail.
        let (f, store) = factory_with(server.uri()).await;
        store
            .put_installation("other_barry", "acme", Some(99), crate::util::now_ts())
            .await
            .unwrap();
        f.preflight_identity(Identity::OtherBarry, "acme", "widget")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn token_mint_401_invalidates_cache_and_retries() {
        let server = MockServer::start().await;
        // 1) First /repos lookup → 200 with id=99 (seeds positive cache via for_identity).
        // 2) /app/installations/99/access_tokens → 401 (install was removed).
        // 3) After invalidation, /repos lookup is retried → 404 → NotInstalled.
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 99})))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/app/installations/99/access_tokens"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let (f, _store) = factory_with(server.uri()).await;
        match f.for_identity(Identity::OtherBarry, "acme", "widget").await {
            Ok(_) => panic!("expected NotInstalled error"),
            Err(err) => assert!(matches!(err, GhFactoryError::NotInstalled { .. })),
        }
    }
}
