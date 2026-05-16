//! Small shared utilities.

/// Current unix timestamp in seconds.
///
/// Saturates at 0 on the (effectively impossible) case where the system clock
/// is set before the Unix epoch. We use seconds-since-epoch throughout the
/// database schema and APIs; a clock that far off would already have broken
/// every other subsystem, so saturating is a safe last-ditch behavior.
pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
