use crate::checker::multi_review::identity::Identity;
use crate::config::Config;
use crate::llm::LlmClient;
use std::fmt;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Wrapper around an LLM client that acquires a semaphore permit before making calls.
pub struct LlmClientWithSemaphore {
    inner: Arc<dyn LlmClient>,
    semaphore: Arc<Semaphore>,
}

impl LlmClientWithSemaphore {
    pub fn new(inner: Arc<dyn LlmClient>, semaphore: Arc<Semaphore>) -> Self {
        Self { inner, semaphore }
    }
}

#[async_trait::async_trait]
impl LlmClient for LlmClientWithSemaphore {
    async fn complete(
        &self,
        req: &crate::llm::LlmRequest,
    ) -> Result<crate::llm::LlmResponse, crate::llm::LlmError> {
        // Semaphore::close is never called in this codebase, so an AcquireError
        // here would indicate a bug. Surface it as a Shape error rather than
        // panic so a single misuse can't take down the whole worker pool.
        let _permit = self.semaphore.acquire().await.map_err(|e| {
            crate::llm::LlmError::Shape(format!("llm concurrency semaphore closed: {e}"))
        })?;
        self.inner.complete(req).await
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

/// Stands in for an identity with no `[llm.*]` profile.
///
/// Only reachable when `[rack]` is configured, where the reviewers' endpoints
/// are unused and requiring them would mean carrying a remote model's address
/// and key purely to satisfy startup. Anything that does call it gets a message
/// naming the section to add, rather than a panic or a confusing HTTP error.
struct Unconfigured {
    name: &'static str,
}

#[async_trait::async_trait]
impl LlmClient for Unconfigured {
    async fn complete(
        &self,
        _req: &crate::llm::LlmRequest,
    ) -> Result<crate::llm::LlmResponse, crate::llm::LlmError> {
        Err(crate::llm::LlmError::Shape(format!(
            "no [llm.{}] profile is configured, and something asked for one",
            self.name
        )))
    }

    fn name(&self) -> &'static str {
        "unconfigured"
    }
}

pub struct IdentityClients {
    pub barry: Arc<dyn LlmClient>,
    pub other_barry: Arc<dyn LlmClient>,
    pub other_other_barry: Arc<dyn LlmClient>,
    pub judge: Arc<dyn LlmClient>,
    pub barry_max_tokens: u32,
    pub other_barry_max_tokens: u32,
    pub other_other_barry_max_tokens: u32,
    pub judge_max_tokens: u32,
}

impl fmt::Debug for IdentityClients {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdentityClients")
            .field("barry_max_tokens", &self.barry_max_tokens)
            .field("other_barry_max_tokens", &self.other_barry_max_tokens)
            .field(
                "other_other_barry_max_tokens",
                &self.other_other_barry_max_tokens,
            )
            .field("judge_max_tokens", &self.judge_max_tokens)
            .finish_non_exhaustive()
    }
}

impl IdentityClients {
    pub fn for_identity(&self, id: Identity) -> &Arc<dyn LlmClient> {
        match id {
            Identity::Barry => &self.barry,
            Identity::OtherBarry => &self.other_barry,
            Identity::OtherOtherBarry => &self.other_other_barry,
        }
    }
    pub fn max_tokens_for(&self, id: Identity) -> u32 {
        match id {
            Identity::Barry => self.barry_max_tokens,
            Identity::OtherBarry => self.other_barry_max_tokens,
            Identity::OtherOtherBarry => self.other_other_barry_max_tokens,
        }
    }
}

pub fn build(cfg: &Config) -> anyhow::Result<IdentityClients> {
    let http = |timeout_secs: u64| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(anyhow::Error::from)
    };

    let llm_semaphore = Arc::new(Semaphore::new(10));

    // Which profiles must exist is [`crate::config::Config::validate`]'s
    // decision, made once, with the rack config in view. By here a profile is
    // either present or deliberately absent.
    let client = |name: &'static str| -> anyhow::Result<Arc<dyn LlmClient>> {
        let Some(profile) = cfg.llm.get(name) else {
            return Ok(Arc::new(Unconfigured { name }));
        };
        Ok(Arc::new(LlmClientWithSemaphore::new(
            crate::llm::factory::build(profile, http(profile.request_timeout_secs)?)?,
            llm_semaphore.clone(),
        )))
    };
    let max_tokens = |name: &str| {
        cfg.llm
            .get(name)
            .map(|p| p.max_tokens)
            .unwrap_or(crate::config::DEFAULT_MAX_TOKENS)
    };

    Ok(IdentityClients {
        barry: client("barry")?,
        other_barry: client("other_barry")?,
        other_other_barry: client("other_other_barry")?,
        judge: client("judge")?,
        barry_max_tokens: max_tokens("barry"),
        other_barry_max_tokens: max_tokens("other_barry"),
        other_other_barry_max_tokens: max_tokens("other_other_barry"),
        judge_max_tokens: max_tokens("judge"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_succeeds_with_three_local_profiles() {
        let toml = r#"
            [server]
            listen = "0.0.0.0:0"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/k"
            webhook_secret_env = "X"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/k"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/k"
            [storage]
            sqlite_path = "/tmp/x.db"
            [dispatcher]
            [llm.barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:2/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "http://localhost:3/v1"
            model = "x"
            [llm.judge]
            provider = "openai"
            endpoint = "http://localhost:4/v1"
            model = "x"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let _ = build(&cfg).unwrap();
    }

    /// Startup no longer rejects this -- `Config::validate` does, and only when
    /// the rack is not covering it. What `build` must not do is panic or
    /// silently produce a client that talks to the wrong endpoint.
    #[test]
    fn a_missing_profile_becomes_a_client_that_explains_itself() {
        let toml = r#"
            [server]
            listen = "0.0.0.0:0"
            [github.barry]
            app_id = 1
            private_key_path = "/tmp/k"
            webhook_secret_env = "X"
            [github.other_barry]
            app_id = 2
            private_key_path = "/tmp/k"
            [github.other_other_barry]
            app_id = 3
            private_key_path = "/tmp/k"
            [storage]
            sqlite_path = "/tmp/x.db"
            [dispatcher]
            [llm.barry]
            provider = "openai"
            endpoint = "http://localhost:1/v1"
            model = "x"
            [llm.other_barry]
            provider = "openai"
            endpoint = "http://localhost:2/v1"
            model = "x"
            [llm.other_other_barry]
            provider = "openai"
            endpoint = "http://localhost:3/v1"
            model = "x"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let clients = build(&cfg).unwrap();
        let err = tokio_test::block_on(clients.judge.complete(&crate::llm::LlmRequest {
            system: None,
            messages: vec![],
            max_tokens: 1,
            temperature: None,
            response_schema: None,
        }))
        .unwrap_err();
        assert!(
            format!("{err}").contains("[llm.judge]"),
            "unexpected error: {err}"
        );
    }
}
