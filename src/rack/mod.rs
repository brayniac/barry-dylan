//! Producing a review on the rack rather than against a local endpoint.
//!
//! Barry submits a systemslab experiment, waits for it, and reads the review
//! back as an artifact. The model runs in an ephemeral VM on a GPU host that is
//! released the moment the review is done, which is the point: a resident
//! `llama-server` would hold a measurement host out of the pool permanently.
//!
//! Barry never speaks to the model. It asks for a review and gets a
//! [`UnifiedReview`] back, so the GPU host needs no GitHub credentials and the
//! guest is free to be destroyed.

pub mod config;
pub mod job;

pub use config::RackConfig;

use crate::checker::multi_review::review::UnifiedReview;
use crate::github::pr::ChangedFile;
use serde::Deserialize;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum RackError {
    #[error("submitting the experiment: {0}")]
    Submit(String),
    #[error("polling experiment {id}: {cause}")]
    Poll { id: String, cause: String },
    #[error("experiment {id} finished as {state}")]
    Failed { id: String, state: String },
    #[error("experiment {id} did not finish within {secs}s (last state: {state})")]
    Timeout {
        id: String,
        secs: u64,
        state: String,
    },
    #[error("experiment {id} produced no `{name}` artifact")]
    MissingArtifact { id: String, name: String },
    #[error("the review returned by experiment {id} was not a valid review: {cause}")]
    BadReview { id: String, cause: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Where an experiment has got to.
///
/// systemslab's states are strings; this reduces them to the only question the
/// caller has, which is whether to keep waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Waiting,
    Succeeded,
    /// Terminal and not success — failure, cancellation, or anything new that
    /// systemslab grows later.
    Ended(String),
}

/// Classify a systemslab experiment state.
///
/// Unknown states count as terminal rather than as "keep waiting". An
/// unrecognised state is far more likely to be a new terminal outcome than a
/// new in-flight one, and treating it as in-flight would hang until the
/// timeout and then report the wrong reason.
pub fn classify(state: &str) -> Progress {
    match state {
        "pending" | "running" => Progress::Waiting,
        "success" => Progress::Succeeded,
        other => Progress::Ended(other.to_string()),
    }
}

#[derive(Deserialize)]
struct SubmitResponse {
    id: String,
}

#[derive(Deserialize)]
struct ExperimentState {
    state: String,
}

#[derive(Deserialize)]
struct ArtifactRef {
    id: String,
    name: String,
}

/// Pick the artifact carrying the review.
///
/// An experiment can hold artifacts from several steps and from systemslab
/// itself, and names are not unique across runs, so the most recent match wins.
fn find_artifact<'a>(artifacts: &'a [ArtifactRef], name: &str) -> Option<&'a ArtifactRef> {
    artifacts.iter().rev().find(|a| a.name == name)
}

/// Run a review on the rack and return it.
pub async fn review(
    cfg: &RackConfig,
    http: &reqwest::Client,
    files: &[ChangedFile],
    name: &str,
) -> Result<UnifiedReview, RackError> {
    if files.is_empty() {
        return Err(RackError::Other(anyhow::anyhow!(
            "no changed files supplied; nothing to review"
        )));
    }

    let base = cfg.systemslab.trim_end_matches('/');
    let spec = job::spec(cfg, files, name)?;

    let resp = http
        .post(format!("{base}/api/v1/submit"))
        .json(&spec)
        .send()
        .await
        .map_err(|e| RackError::Submit(e.to_string()))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| RackError::Submit(e.to_string()))?;
    if !status.is_success() {
        return Err(RackError::Submit(format!("{status}: {body}")));
    }
    let submitted: SubmitResponse =
        serde_json::from_str(&body).map_err(|e| RackError::Submit(format!("{e}: {body}")))?;
    let id = submitted.id;

    tracing::info!(experiment = %id, shape = %cfg.shape, "rack review submitted");

    let deadline = Instant::now() + Duration::from_secs(cfg.job_timeout_secs);
    let mut last = String::from("unknown");
    loop {
        if Instant::now() >= deadline {
            return Err(RackError::Timeout {
                id,
                secs: cfg.job_timeout_secs,
                state: last,
            });
        }

        let state: ExperimentState = http
            .get(format!("{base}/api/v1/experiment/{id}"))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| RackError::Poll {
                id: id.clone(),
                cause: e.to_string(),
            })?
            .json()
            .await
            .map_err(|e| RackError::Poll {
                id: id.clone(),
                cause: e.to_string(),
            })?;

        if state.state != last {
            tracing::info!(experiment = %id, state = %state.state, "rack review progress");
            last = state.state.clone();
        }

        match classify(&state.state) {
            Progress::Succeeded => break,
            Progress::Ended(s) => return Err(RackError::Failed { id, state: s }),
            Progress::Waiting => {
                tokio::time::sleep(Duration::from_secs(cfg.poll_interval_secs)).await;
            }
        }
    }

    let artifacts: Vec<ArtifactRef> = http
        .get(format!("{base}/api/v1/experiment/{id}/artifacts"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| RackError::Poll {
            id: id.clone(),
            cause: e.to_string(),
        })?
        .json()
        .await
        .map_err(|e| RackError::Poll {
            id: id.clone(),
            cause: e.to_string(),
        })?;

    let artifact = find_artifact(&artifacts, job::REVIEW_ARTIFACT).ok_or_else(|| {
        RackError::MissingArtifact {
            id: id.clone(),
            name: job::REVIEW_ARTIFACT.to_string(),
        }
    })?;

    let text = http
        .get(format!("{base}/api/v1/artifact/{}", artifact.id))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| RackError::Poll {
            id: id.clone(),
            cause: e.to_string(),
        })?
        .text()
        .await
        .map_err(|e| RackError::Poll {
            id: id.clone(),
            cause: e.to_string(),
        })?;

    serde_json::from_str(&text).map_err(|e| RackError::BadReview {
        id,
        cause: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_states_keep_waiting() {
        assert_eq!(classify("pending"), Progress::Waiting);
        assert_eq!(classify("running"), Progress::Waiting);
    }

    #[test]
    fn success_is_the_only_success() {
        assert_eq!(classify("success"), Progress::Succeeded);
        assert_eq!(classify("failure"), Progress::Ended("failure".into()));
        assert_eq!(classify("cancelled"), Progress::Ended("cancelled".into()));
    }

    #[test]
    fn an_unknown_state_is_terminal_not_in_flight() {
        // Treating an unrecognised state as in-flight would wait out the whole
        // timeout and then blame the timeout for something that had already
        // finished.
        assert_eq!(
            classify("something_new"),
            Progress::Ended("something_new".into())
        );
    }

    fn artifact(id: &str, name: &str) -> ArtifactRef {
        ArtifactRef {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn the_review_artifact_is_found_among_others() {
        let all = vec![
            artifact("1", "metrics.rez"),
            artifact("2", "review.json"),
            artifact("3", "rezolus.rez"),
        ];
        assert_eq!(find_artifact(&all, "review.json").unwrap().id, "2");
    }

    #[test]
    fn the_most_recent_artifact_of_a_name_wins() {
        // Names are not unique across runs of the same experiment.
        let all = vec![
            artifact("old", "review.json"),
            artifact("new", "review.json"),
        ];
        assert_eq!(find_artifact(&all, "review.json").unwrap().id, "new");
    }

    #[test]
    fn a_missing_artifact_is_none_not_a_panic() {
        let all = vec![artifact("1", "metrics.rez")];
        assert!(find_artifact(&all, "review.json").is_none());
    }
}
