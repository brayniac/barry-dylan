use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct JobProgress {
    pub owner: String,
    pub repo: String,
    pub pr_number: i64,
    pub phase: String,
    pub job_started: Instant,
    pub phase_started: Instant,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

#[derive(Clone, Default)]
pub struct StatusTracker {
    jobs: Arc<RwLock<HashMap<i64, JobProgress>>>,
}

impl StatusTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, job_id: i64, owner: &str, repo: &str, pr_number: i64) {
        let now = Instant::now();
        let mut jobs = self.jobs.write().unwrap();
        jobs.insert(
            job_id,
            JobProgress {
                owner: owner.to_string(),
                repo: repo.to_string(),
                pr_number,
                phase: "starting".to_string(),
                job_started: now,
                phase_started: now,
                tokens_in: 0,
                tokens_out: 0,
            },
        );
    }

    pub fn set_phase(&self, job_id: i64, phase: &str) {
        let mut jobs = self.jobs.write().unwrap();
        if let Some(p) = jobs.get_mut(&job_id) {
            p.phase = phase.to_string();
            p.phase_started = Instant::now();
        }
    }

    pub fn add_tokens(&self, job_id: i64, input: u64, output: u64) {
        let mut jobs = self.jobs.write().unwrap();
        if let Some(p) = jobs.get_mut(&job_id) {
            p.tokens_in += input;
            p.tokens_out += output;
        }
    }

    pub fn complete(&self, job_id: i64) {
        let mut jobs = self.jobs.write().unwrap();
        jobs.remove(&job_id);
    }

    pub fn snapshot(&self) -> Vec<JobProgress> {
        let jobs = self.jobs.read().unwrap();
        jobs.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_creates_entry_with_correct_metadata() {
        let t = StatusTracker::new();
        t.begin(1, "acme", "my-repo", 42);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].owner, "acme");
        assert_eq!(snap[0].repo, "my-repo");
        assert_eq!(snap[0].pr_number, 42);
        assert_eq!(snap[0].phase, "starting");
        assert_eq!(snap[0].tokens_in, 0);
        assert_eq!(snap[0].tokens_out, 0);
    }

    #[test]
    fn set_phase_updates_name() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.set_phase(1, "R1 synthesis");
        let snap = t.snapshot();
        assert_eq!(snap[0].phase, "R1 synthesis");
    }

    #[test]
    fn add_tokens_accumulates_across_calls() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.add_tokens(1, 100, 50);
        t.add_tokens(1, 200, 75);
        let snap = t.snapshot();
        assert_eq!(snap[0].tokens_in, 300);
        assert_eq!(snap[0].tokens_out, 125);
    }

    #[test]
    fn complete_removes_entry() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.complete(1);
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn unknown_job_id_is_silently_ignored() {
        let t = StatusTracker::new();
        t.set_phase(999, "whatever");
        t.add_tokens(999, 1, 1);
        t.complete(999);
        // no panic
    }

    #[test]
    fn snapshot_returns_all_active_jobs() {
        let t = StatusTracker::new();
        t.begin(1, "o", "r", 1);
        t.begin(2, "o", "r", 2);
        assert_eq!(t.snapshot().len(), 2);
    }
}
