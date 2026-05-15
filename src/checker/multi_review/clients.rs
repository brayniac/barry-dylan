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
        let _permit = self.semaphore.acquire().await.expect("semaphore closed");
        self.inner.complete(req).await
    }

    fn name(&self) -> &'static str {
        self.inner.name()
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

    let pick = |name: &str| -> anyhow::Result<&crate::config::LlmProfile> {
        cfg.llm
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("missing [llm.{name}]"))
    };

    let b = pick("barry")?;
    let ob = pick("other_barry")?;
    let oob = pick("other_other_barry")?;
    let judge = pick("judge")?;

    let llm_semaphore = Arc::new(Semaphore::new(10));

    let clients = IdentityClients {
        barry: Arc::new(LlmClientWithSemaphore::new(
            crate::llm::factory::build(b, http(b.request_timeout_secs)?)?,
            llm_semaphore.clone(),
        )),
        other_barry: Arc::new(LlmClientWithSemaphore::new(
            crate::llm::factory::build(ob, http(ob.request_timeout_secs)?)?,
            llm_semaphore.clone(),
        )),
        other_other_barry: Arc::new(LlmClientWithSemaphore::new(
            crate::llm::factory::build(oob, http(oob.request_timeout_secs)?)?,
            llm_semaphore.clone(),
        )),
        judge: Arc::new(LlmClientWithSemaphore::new(
            crate::llm::factory::build(judge, http(judge.request_timeout_secs)?)?,
            llm_semaphore,
        )),
        barry_max_tokens: b.max_tokens,
        other_barry_max_tokens: ob.max_tokens,
        other_other_barry_max_tokens: oob.max_tokens,
        judge_max_tokens: judge.max_tokens,
    };
    Ok(clients)
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

    #[test]
    fn build_rejects_missing_judge() {
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
        let err = build(&cfg).unwrap_err();
        assert!(format!("{err}").contains("judge"));
    }
}
