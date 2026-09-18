pub mod config;

use crate::checker::multi_review::clients::IdentityClients;
use crate::checker::multi_review::orchestrator::{Orchestrator, Verdict};
use crate::checker::multi_review::persona;
use crate::checker::multi_review::review::UnifiedReview;
use crate::config::LlmProfile;
use crate::github::pr::ChangedFile;
use crate::offline::config::OfflineConfig;
use crate::telemetry::status::StatusTracker;
use std::sync::Arc;

/// Job ID used for the offline run. The status tracker is in-memory and
/// discarded, so any value works; zero makes offline runs obvious in logs.
const OFFLINE_JOB_ID: i64 = 0;

/// Build an [`IdentityClients`] where every slot is the same endpoint.
///
/// Offline review calls `Orchestrator::run_barry_only`, which touches only the
/// Barry slot — no peer review, no judge. The other slots are populated because
/// the struct requires them, not because they are used.
///
/// Unlike [`crate::checker::multi_review::clients::build`], this does not wrap
/// clients in a semaphore: offline runs one review at a time against one
/// endpoint, so there is no concurrency to bound.
fn clients_from_profile(profile: &LlmProfile) -> anyhow::Result<IdentityClients> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(profile.request_timeout_secs))
        .build()?;
    let client = crate::llm::factory::build(profile, http)?;
    Ok(IdentityClients {
        barry: client.clone(),
        other_barry: client.clone(),
        other_other_barry: client.clone(),
        judge: client,
        barry_max_tokens: profile.max_tokens,
        other_barry_max_tokens: profile.max_tokens,
        other_other_barry_max_tokens: profile.max_tokens,
        judge_max_tokens: profile.max_tokens,
        barry_context_size: profile.context_size,
        other_barry_context_size: profile.context_size,
        other_other_barry_context_size: profile.context_size,
    })
}

/// Run the local-model half of the review pipeline and return the review.
///
/// This is persona drafts plus R1 synthesis for a single identity. There is no
/// peer review, no judge, and nothing is posted anywhere — the caller decides
/// what to do with the result.
pub async fn run(cfg: &OfflineConfig, files: &[ChangedFile]) -> anyhow::Result<UnifiedReview> {
    if files.is_empty() {
        anyhow::bail!("no changed files supplied; nothing to review");
    }

    let clients = clients_from_profile(&cfg.llm)?;
    let personas = persona::resolve(&persona::overrides_from_config(&cfg.personas))?;

    let tracker = Arc::new(StatusTracker::new());
    tracker.begin(OFFLINE_JOB_ID, "offline", "offline", 0);

    let orchestrator = Orchestrator {
        clients: &clients,
        personas: &personas,
        tracker,
        job_id: OFFLINE_JOB_ID,
    };

    let verdict = orchestrator
        .run_barry_only(files, "offline mode".to_string())
        .await?;

    // run_barry_only only ever returns BarryAlone, but Verdict is a public enum
    // and matching exhaustively means a new variant becomes a compile error
    // here rather than a silent wrong answer.
    match verdict {
        Verdict::BarryAlone { barry, .. } => Ok(barry),
        Verdict::Agree { barry } => Ok(barry),
        Verdict::Disagree { barry, .. } => Ok(barry),
    }
}

/// Reconcile two finished reviews against the offline model.
///
/// The counterpart to [`run`] for the rack: the guest that produced both
/// reviews still has the weights loaded, so it can answer "do these two
/// materially agree" without delta holding an LLM credential at all.
///
/// Capped at 512 output tokens like the in-process judge — a verdict is a
/// boolean and a sentence, and a judge given room to ramble writes an essay
/// instead of an answer.
pub async fn judge(
    cfg: &OfflineConfig,
    a: &UnifiedReview,
    b: &UnifiedReview,
) -> anyhow::Result<crate::rack::RackVerdict> {
    let clients = clients_from_profile(&cfg.llm)?;
    let verdict = crate::checker::multi_review::judge::judge(
        clients.judge.as_ref(),
        a,
        b,
        // The guest's `[llm] max_tokens` is `[rack] judge_max_tokens`, raised
        // to 4096 on 2026-09-16 for exactly this model class. The `.min(512)`
        // that sat here clamped it straight back, which is why the rack judge
        // kept failing after the fix (barry-dylan#35, 2026-09-18).
        cfg.llm.max_tokens,
    )
    .await?;
    Ok(crate::rack::RackVerdict {
        agree: verdict.agree,
        reason: verdict.reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::multi_review::identity::Identity;
    use crate::config::{LlmProfile, LlmProviderKind};

    fn profile() -> LlmProfile {
        LlmProfile {
            provider: LlmProviderKind::Openai,
            endpoint: "http://127.0.0.1:1/v1".into(),
            api_key_env: None,
            model: "test-model".into(),
            max_tokens: 4096,
            request_timeout_secs: 5,
            context_size: None,
            temperature: None,
            structured_output: None,
        }
    }

    #[test]
    fn every_identity_slot_gets_the_same_client() {
        let clients = clients_from_profile(&profile()).unwrap();
        assert_eq!(clients.max_tokens_for(Identity::Barry), 4096);
        assert_eq!(clients.max_tokens_for(Identity::OtherBarry), 4096);
    }
}
