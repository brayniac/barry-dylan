/// Compute the run_after timestamp for a new job, given an existing pending
/// run_after (if any), the current time, and the debounce window. Always
/// returns max(now+debounce, existing_run_after) so debounce never shrinks.
pub fn next_run_after(existing: Option<i64>, now_ts: i64, debounce_secs: i64) -> i64 {
    let proposed = now_ts + debounce_secs;
    existing.map(|e| e.max(proposed)).unwrap_or(proposed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_existing_uses_now_plus_debounce() {
        assert_eq!(next_run_after(None, 100, 30), 130);
    }

    #[test]
    fn extends_when_existing_earlier() {
        assert_eq!(next_run_after(Some(120), 110, 30), 140);
    }

    #[test]
    fn keeps_existing_when_later() {
        assert_eq!(next_run_after(Some(200), 110, 30), 200);
    }
}
