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

pub use config::{Placement, RackConfig, Reviewer, VERDICT_ARTIFACT};

use crate::checker::multi_review::review::UnifiedReview;
use crate::github::pr::ChangedFile;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

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
    #[error(
        "the rack was busy: experiment {id} waited {secs}s without being given a host, \
         so the review was cancelled rather than queued behind measurement work"
    )]
    Queued { id: String, secs: u64 },
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
    /// Per-job states. The experiment-level state says `pending` both while
    /// nothing has been scheduled and while a guest is mid-review, so it cannot
    /// answer "has this started"; only the jobs can.
    #[serde(default)]
    jobs: Vec<JobState>,
}

#[derive(Deserialize)]
struct JobState {
    state: String,
}

/// Has the rack given this experiment a host yet?
///
/// Anything other than `unscheduled` counts as started, including terminal
/// states: a job that has already failed has certainly been scheduled, and
/// treating it as still queued would cancel it for the wrong reason.
fn has_started(jobs: &[JobState]) -> bool {
    jobs.iter().any(|j| j.state != "unscheduled")
}

/// Ask the rack to stop, and do not care very much whether it agrees.
///
/// Best effort by design, as rack-ci's equivalent is: this is called when
/// barry has already stopped waiting, and an experiment that has just been
/// scheduled will refuse. Failing the review because the cancellation failed
/// would be the wrong trade -- the review has failed either way, and the
/// point of cancelling is to stop a GPU host producing an answer nobody is
/// waiting for.
async fn cancel(http: &reqwest::Client, base: &str, id: &str) {
    let url = format!("{base}/api/v1/experiment/{id}/cancel");
    match http.post(&url).send().await {
        Ok(r) if r.status().is_success() => {
            tracing::info!(experiment = %id, "cancelled a review nobody is waiting for")
        }
        Ok(r) => tracing::warn!(experiment = %id, status = %r.status(), "could not cancel"),
        Err(e) => tracing::warn!(experiment = %id, error = %e, "could not cancel"),
    }
}

/// Cancels the experiment if the review is abandoned before it ends.
///
/// `cancel` above covers the two ways [`review`] gives up on its own. It does
/// not cover the ways the *caller* gives up: the dispatcher drops the review
/// future when the pull request is closed and when the checker times out, and
/// a dropped future runs no more code. The first real case was infra#18 on
/// 2026-09-18: closed at 16:22Z, barry stopped waiting at 16:22Z, and the guest
/// held hv02 until 16:28Z producing a review nobody would read, while two
/// measurement jobs starved behind it.
///
/// Armed once the experiment exists; disarmed once it is over or once the
/// review has decided to cancel it explicitly. `Drop` cannot await, so the
/// request is spawned, and the worker task that dropped us outlives it.
///
/// One drop is not abandonment: barry stopping. The experiment is recorded on
/// the job (#39) and the next process resumes it, so cancelling it would throw
/// away GPU work that was about to succeed. `shutdown` tells the two apart.
struct Abandoned {
    http: reqwest::Client,
    base: String,
    id: String,
    armed: bool,
    shutdown: CancellationToken,
}

impl Abandoned {
    fn arm(http: reqwest::Client, base: String, id: String, shutdown: CancellationToken) -> Self {
        Self {
            http,
            base,
            id,
            armed: true,
            shutdown,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Abandoned {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.shutdown.is_cancelled() {
            tracing::info!(
                experiment = %self.id,
                "barry is stopping; leaving the experiment running for the next process to resume"
            );
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(experiment = %self.id, "abandoned outside a runtime; cannot cancel");
            return;
        };
        tracing::info!(experiment = %self.id, "review abandoned before the rack finished; cancelling");
        let http = self.http.clone();
        let base = std::mem::take(&mut self.base);
        let id = std::mem::take(&mut self.id);
        rt.spawn(async move { cancel(&http, &base, &id).await });
    }
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

/// One review per configured reviewer, keyed by identity slug.
pub type Reviews = std::collections::BTreeMap<String, UnifiedReview>;

/// What the in-guest judge decided, when it ran.
///
/// Deliberately not [`crate::checker::multi_review::judge::JudgeVerdict`]: that
/// carries a token count, which belongs to the process that spent the tokens.
/// This crosses a job boundary as JSON and should carry only the decision.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RackVerdict {
    pub agree: bool,
    #[serde(default)]
    pub reason: String,
}

/// What a rack job produced.
///
/// `verdict` is `None` whenever the guest was not asked to judge, or was asked
/// and could not -- a judge that fails writes an error into its artifact rather
/// than failing the job, because two good reviews are worth more than the
/// reconciliation. The caller reconciles them itself in that case.
#[derive(Debug)]
pub struct RackOutcome {
    pub reviews: Reviews,
    pub verdict: Option<RackVerdict>,
}

/// Submit the experiment for every configured reviewer and return its id.
pub async fn submit(
    cfg: &RackConfig,
    http: &reqwest::Client,
    files: &[ChangedFile],
    name: &str,
) -> Result<String, RackError> {
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

    tracing::info!(
        experiment = %submitted.id,
        placement = ?cfg.placement,
        reviewers = cfg.reviewers.len(),
        "rack review submitted"
    );
    Ok(submitted.id)
}

/// Wait for experiment `id` and collect its reviews.
///
/// Works for an experiment this process submitted a moment ago and for one a
/// previous process submitted before it stopped (#39): the rack does not care
/// who is asking. The timeouts count from now either way, which for a resumed
/// experiment is generous by however long it had already run.
pub async fn collect(
    cfg: &RackConfig,
    http: &reqwest::Client,
    id: &str,
    shutdown: &CancellationToken,
) -> Result<RackOutcome, RackError> {
    let base = cfg.systemslab.trim_end_matches('/');
    let id = id.to_string();
    let mut abandoned =
        Abandoned::arm(http.clone(), base.to_string(), id.clone(), shutdown.clone());

    let deadline = Instant::now() + Duration::from_secs(cfg.job_timeout_secs);
    let queue_deadline = Instant::now() + Duration::from_secs(cfg.queue_timeout_secs);
    let mut started = false;
    let mut last = String::from("unknown");
    loop {
        if Instant::now() >= deadline {
            abandoned.disarm();
            cancel(http, base, &id).await;
            return Err(RackError::Timeout {
                id,
                secs: cfg.job_timeout_secs,
                state: last,
            });
        }
        if !started && Instant::now() >= queue_deadline {
            abandoned.disarm();
            cancel(http, base, &id).await;
            return Err(RackError::Queued {
                id,
                secs: cfg.queue_timeout_secs,
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

        if !started && has_started(&state.jobs) {
            started = true;
            tracing::info!(experiment = %id, "the rack gave the review a host");
        }
        if state.state != last {
            tracing::info!(experiment = %id, state = %state.state, "rack review progress");
            last = state.state.clone();
        }

        match classify(&state.state) {
            Progress::Succeeded => {
                abandoned.disarm();
                break;
            }
            Progress::Ended(s) => {
                abandoned.disarm();
                return Err(RackError::Failed { id, state: s });
            }
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

    // Every reviewer must have produced a review. A partial set would let a
    // judge compare one real review against nothing and call it agreement.
    let mut reviews = Reviews::new();
    for r in &cfg.reviewers {
        let want = r.artifact();
        let artifact =
            find_artifact(&artifacts, &want).ok_or_else(|| RackError::MissingArtifact {
                id: id.clone(),
                name: want.clone(),
            })?;

        let text = fetch_artifact(http, base, &id, &artifact.id).await?;

        let review: UnifiedReview =
            serde_json::from_str(&text).map_err(|e| RackError::BadReview {
                id: id.clone(),
                cause: format!("{want}: {e}"),
            })?;
        reviews.insert(r.identity.clone(), review);
    }

    // The verdict, when one was asked for. Unlike a review, a missing or
    // unparseable verdict is not an error: the caller still has both reviews
    // and can reconcile them itself, which is what it did before the guest
    // could judge at all.
    let verdict = if cfg.judge {
        read_verdict(http, base, &id, &artifacts).await
    } else {
        None
    };

    Ok(RackOutcome { reviews, verdict })
}

/// Run every configured reviewer on the rack and return their reviews.
///
/// One shot, nothing remembered: for `review-rack` on the command line. The
/// dispatcher uses [`review_for_job`], which survives a restart.
pub async fn review(
    cfg: &RackConfig,
    http: &reqwest::Client,
    files: &[ChangedFile],
    name: &str,
) -> Result<RackOutcome, RackError> {
    let id = submit(cfg, http, files, name).await?;
    collect(cfg, http, &id, &CancellationToken::new()).await
}

/// Review on the rack for a dispatcher job, resuming the job's experiment if
/// a previous process left one behind (#39).
///
/// The experiment id is written to the job row the moment it exists and
/// cleared once the experiment is over. If barry stops in between, the future
/// is dropped without running the clearing line, the abandon guard sees the
/// shutdown and leaves the experiment alone, and the job comes back to the
/// next process still carrying the id. That process polls the same experiment
/// instead of paying for another. A resumed experiment that ended badly is
/// resubmitted once, since its reviewers may not match today's config any
/// more than its outcome does.
pub async fn review_for_job(
    cfg: &RackConfig,
    http: &reqwest::Client,
    files: &[ChangedFile],
    name: &str,
    store: &crate::storage::Store,
    job_id: i64,
    shutdown: &CancellationToken,
) -> Result<RackOutcome, RackError> {
    let remembered = store
        .rack_experiment(job_id)
        .await
        .map_err(RackError::Other)?;
    let (id, resumed) = match remembered {
        Some(id) => {
            tracing::info!(
                experiment = %id,
                job_id,
                "resuming the experiment a previous process submitted"
            );
            metrics::counter!("barry_rack_resumed_total").increment(1);
            (id, true)
        }
        None => {
            let id = submit(cfg, http, files, name).await?;
            store
                .set_rack_experiment(job_id, Some(&id))
                .await
                .map_err(RackError::Other)?;
            (id, false)
        }
    };

    let mut outcome = collect(cfg, http, &id, shutdown).await;
    if resumed
        && matches!(
            outcome,
            Err(RackError::Failed { .. }
                | RackError::MissingArtifact { .. }
                | RackError::BadReview { .. })
        )
    {
        let why = outcome
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        tracing::warn!(experiment = %id, job_id, error = %why, "the resumed experiment is no use; submitting afresh");
        let id = submit(cfg, http, files, name).await?;
        store
            .set_rack_experiment(job_id, Some(&id))
            .await
            .map_err(RackError::Other)?;
        outcome = collect(cfg, http, &id, shutdown).await;
    }

    // Terminal either way. If we were dropped instead, this never runs and the
    // id stays on the job, which is the point.
    if let Err(e) = store.set_rack_experiment(job_id, None).await {
        tracing::warn!(?e, job_id, "could not clear the job's rack experiment");
    }
    outcome
}

/// Fetch one artifact's body.
async fn fetch_artifact(
    http: &reqwest::Client,
    base: &str,
    experiment: &str,
    artifact_id: &str,
) -> Result<String, RackError> {
    http.get(format!("{base}/api/v1/artifact/{artifact_id}"))
        .send()
        .await
        .and_then(|resp| resp.error_for_status())
        .map_err(|e| RackError::Poll {
            id: experiment.to_string(),
            cause: e.to_string(),
        })?
        .text()
        .await
        .map_err(|e| RackError::Poll {
            id: experiment.to_string(),
            cause: e.to_string(),
        })
}

/// Read the in-guest verdict, or explain in the log why there is none.
///
/// Every failure here degrades to `None` rather than propagating: the reviews
/// are already in hand, and losing them to a failed reconciliation would be a
/// far worse outcome than reconciling on delta after all.
async fn read_verdict(
    http: &reqwest::Client,
    base: &str,
    experiment: &str,
    artifacts: &[ArtifactRef],
) -> Option<RackVerdict> {
    let outcome = |o: &str| {
        metrics::counter!("barry_rack_judge_total", "outcome" => o.to_string()).increment(1)
    };

    let Some(artifact) = find_artifact(artifacts, VERDICT_ARTIFACT) else {
        tracing::warn!(
            experiment = %experiment,
            "no {VERDICT_ARTIFACT}; reconciling here instead"
        );
        outcome("missing");
        return None;
    };
    let text = match fetch_artifact(http, base, experiment, &artifact.id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(experiment = %experiment, error = %e, "could not read the verdict");
            outcome("missing");
            return None;
        }
    };
    match serde_json::from_str::<RackVerdict>(&text) {
        Ok(v) => {
            tracing::info!(
                experiment = %experiment,
                agree = v.agree,
                reason = %v.reason,
                "judged on the rack"
            );
            outcome("verdict");
            Some(v)
        }
        Err(e) => {
            // The guest writes `{"error": ...}` here when judge-offline failed,
            // so this is the expected shape of an in-guest judge failure, not a
            // surprise.
            let head: String = text.chars().take(200).collect();
            tracing::warn!(
                experiment = %experiment,
                error = %e,
                verdict = %head,
                "unusable verdict; reconciling here instead"
            );
            outcome("invalid");
            None
        }
    }
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

    fn jobs(states: &[&str]) -> Vec<JobState> {
        states
            .iter()
            .map(|s| JobState {
                state: (*s).to_string(),
            })
            .collect()
    }

    #[test]
    fn an_unscheduled_job_has_not_started() {
        // The experiment reads `pending` here too, which is why the job states
        // are what this asks.
        assert!(!has_started(&jobs(&["unscheduled"])));
    }

    #[test]
    fn a_running_job_has_started() {
        assert!(has_started(&jobs(&["running"])));
    }

    #[test]
    fn a_job_that_already_failed_counts_as_started() {
        // It certainly got a host. Treating it as still queued would cancel it
        // for the wrong reason and report the wrong thing.
        assert!(has_started(&jobs(&["failed"])));
    }

    #[test]
    fn one_scheduled_job_is_enough() {
        assert!(has_started(&jobs(&["unscheduled", "running"])));
    }

    #[test]
    fn an_experiment_with_no_jobs_yet_has_not_started() {
        assert!(!has_started(&[]));
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg_for(server: &MockServer) -> RackConfig {
        toml::from_str(&format!(
            r#"
systemslab = "{}"
host_tags = ["gpu"]
shape = "auto.g"
image = "spool/images/debian-13-gpu@golden"
poll_interval_secs = 1

[[reviewers]]
identity = "barry"
model = "x/y@latest/gguf/q4_k_m@latest"
model_name = "y"
"#,
            server.uri()
        ))
        .unwrap()
    }

    fn one_file() -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 1,
            deletions: 0,
            changes: 1,
            patch: Some("@@ -1 +1 @@\n+x".into()),
        }]
    }

    async fn cancels_received(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path().ends_with("/cancel"))
            .count()
    }

    #[tokio::test]
    async fn a_review_dropped_mid_flight_cancels_the_experiment() {
        // The dispatcher drops the review future when the pull request is
        // closed or the checker times out. The guest must not keep the host.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/submit"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "01a0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/01a0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "pending", "jobs": [{"state": "running"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/experiment/01a0/cancel"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();

        // Poll once, then abandon it the way `select!` on a cancel token does.
        let dropped = tokio::time::timeout(
            Duration::from_millis(300),
            review(&cfg, &http, &one_file(), "t"),
        )
        .await;
        assert!(
            dropped.is_err(),
            "the review should still have been waiting"
        );

        for _ in 0..40 {
            if cancels_received(&server).await == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the experiment was not cancelled after the review was dropped");
    }

    #[tokio::test]
    async fn a_review_dropped_because_barry_is_stopping_leaves_the_experiment_alone() {
        // The next process resumes it (#39). Cancelling would throw away the
        // GPU work done so far.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/01a0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "pending", "jobs": [{"state": "running"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/experiment/01a0/cancel"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();
        let shutdown = CancellationToken::new();

        let waiting = collect(&cfg, &http, "01a0", &shutdown);
        tokio::pin!(waiting);
        // Let it poll once, then stop barry and drop the review as the worker
        // does.
        let _ = tokio::time::timeout(Duration::from_millis(300), &mut waiting).await;
        shutdown.cancel();
        drop(waiting);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(cancels_received(&server).await, 0);
    }

    /// Mocks for an experiment that has succeeded with one review artifact.
    async fn mount_succeeded(server: &MockServer, id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/experiment/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "success", "jobs": [{"state": "complete"}]}),
            ))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/experiment/{id}/artifacts")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!([{"id": format!("art-{id}"), "name": "review-barry.json"}]),
            ))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/artifact/art-{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"outcome":"approve","summary":"LGTM","findings":[]}"#),
            )
            .mount(server)
            .await;
    }

    async fn submits_received(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/api/v1/submit")
            .count()
    }

    async fn leased_job(store: &crate::storage::Store) -> i64 {
        store
            .enqueue(
                &crate::storage::queue::NewJob {
                    installation_id: 1,
                    repo_owner: "o".into(),
                    repo_name: "r".into(),
                    pr_number: 1,
                    event_kind: "pull_request.opened".into(),
                    delivery_id: "d".into(),
                    actor: None,
                },
                0,
                0,
            )
            .await
            .unwrap();
        store.lease_next(1, 300).await.unwrap().unwrap().id
    }

    #[tokio::test]
    async fn a_job_left_with_an_experiment_resumes_it_instead_of_submitting() {
        let server = MockServer::start().await;
        mount_succeeded(&server, "old1").await;
        // No submit mock at all: a submit would 404 and fail the review.
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();
        let store = crate::storage::Store::in_memory().await.unwrap();
        let job = leased_job(&store).await;
        store.set_rack_experiment(job, Some("old1")).await.unwrap();

        let out = review_for_job(
            &cfg,
            &http,
            &one_file(),
            "t",
            &store,
            job,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.reviews.contains_key("barry"));
        assert_eq!(submits_received(&server).await, 0);
        // Over now; the next lease of this job must not resume a finished one.
        assert_eq!(store.rack_experiment(job).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_fresh_job_submits_and_remembers_the_experiment_while_waiting() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/submit"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "new1"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/new1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "pending", "jobs": [{"state": "running"}]}),
            ))
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();
        let store = crate::storage::Store::in_memory().await.unwrap();
        let job = leased_job(&store).await;
        let shutdown = CancellationToken::new();

        // Stop barry while it is waiting: the id must be on the job for the
        // next process, and the experiment must be left alone.
        let waiting = review_for_job(&cfg, &http, &one_file(), "t", &store, job, &shutdown);
        tokio::pin!(waiting);
        let _ = tokio::time::timeout(Duration::from_millis(300), &mut waiting).await;
        shutdown.cancel();
        drop(waiting);

        assert_eq!(
            store.rack_experiment(job).await.unwrap().as_deref(),
            Some("new1")
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(cancels_received(&server).await, 0);
    }

    #[tokio::test]
    async fn a_resumed_experiment_that_ended_badly_is_submitted_afresh_once() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/old1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "failure", "jobs": [{"state": "failed"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/submit"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "new1"})),
            )
            .mount(&server)
            .await;
        mount_succeeded(&server, "new1").await;
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();
        let store = crate::storage::Store::in_memory().await.unwrap();
        let job = leased_job(&store).await;
        store.set_rack_experiment(job, Some("old1")).await.unwrap();

        let out = review_for_job(
            &cfg,
            &http,
            &one_file(),
            "t",
            &store,
            job,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.reviews.contains_key("barry"));
        assert_eq!(submits_received(&server).await, 1);
        assert_eq!(store.rack_experiment(job).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_review_that_ends_on_its_own_is_not_cancelled() {
        // Cancelling something already finished is noise at best; the guard
        // must stand down once the experiment has reached a terminal state.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/submit"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "01a0"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/experiment/01a0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"state": "failure", "jobs": [{"state": "failed"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/experiment/01a0/cancel"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let http = reqwest::Client::new();

        let err = review(&cfg, &http, &one_file(), "t").await.unwrap_err();
        assert!(matches!(err, RackError::Failed { .. }), "{err}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(cancels_received(&server).await, 0);
    }

    #[test]
    fn a_queued_out_review_says_the_rack_was_busy() {
        // What lands on the pull request. "internal error" would not tell
        // anyone that the answer is to wait rather than to debug barry.
        let e = RackError::Queued {
            id: "01a0".into(),
            secs: 900,
        };
        let msg = e.to_string();
        assert!(msg.contains("the rack was busy"), "{msg}");
        assert!(msg.contains("900s"), "{msg}");
    }
}
