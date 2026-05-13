use crate::dispatcher::run::{run_job, JobDeps};
use std::sync::Arc;
use std::time::Duration;

pub async fn run_worker(deps: Arc<JobDeps>, lease_secs: i64) {
    let backoff = [60i64, 300, 1500];
    loop {
        let now = now_ts();
        let leased = match deps.store.lease_next(now, lease_secs).await {
            Ok(Some(j)) => j,
            Ok(None) => { tokio::time::sleep(Duration::from_secs(1)).await; continue; }
            Err(e) => {
                tracing::error!(?e, "lease_next failed");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let id = leased.id;
        tracing::info!(
            job_id = id, owner = %leased.repo_owner, repo = %leased.repo_name,
            pr = leased.pr_number, event_kind = %leased.event_kind,
            attempts = leased.attempts, "job leased",
        );
        metrics::counter!("barry_job_leased_total").increment(1);
        let t = std::time::Instant::now();
        let res = run_job(&deps, &leased).await;
        let dur = t.elapsed();
        metrics::histogram!("barry_job_duration_ms").record(dur.as_millis() as f64);

        match res {
            Ok(()) => {
                let _ = deps.store.ack(id).await;
                metrics::counter!("barry_job_completed_total", "outcome" => "success").increment(1);
                tracing::info!(
                    job_id = id, owner = %leased.repo_owner, repo = %leased.repo_name,
                    pr = leased.pr_number, duration_ms = dur.as_millis() as u64,
                    "job completed",
                );
            }
            Err(e) => {
                tracing::error!(
                    ?e, job_id = id, owner = %leased.repo_owner, repo = %leased.repo_name,
                    pr = leased.pr_number, attempts = leased.attempts, "job failed",
                );
                let msg = format!("{e}");
                let alive = deps.store.nack(id, now_ts(), &msg, 3, &backoff).await
                    .unwrap_or(false);
                if alive {
                    metrics::counter!("barry_job_completed_total", "outcome" => "retry").increment(1);
                    tracing::warn!(job_id = id, attempts = leased.attempts + 1, "job scheduled for retry");
                } else {
                    metrics::counter!("barry_job_completed_total", "outcome" => "dropped").increment(1);
                    tracing::error!(job_id = id, attempts = leased.attempts + 1, "job dropped after max attempts");
                }
            }
        }
    }
}

fn now_ts() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}
