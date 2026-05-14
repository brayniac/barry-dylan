//! Job dispatching and worker execution.
//!
//! ## Job Queue
//!
//! Jobs represent work to be done on a PR (e.g., `pull_request` webhook events).
//! The queue supports:
//! - **Leasing**: Atomic `UPDATE...RETURNING` to prevent duplicate processing
//! - **Coalescing**: Multiple events for the same PR are debounced
//! - **Retry**: Failed jobs are retried with exponential backoff
//!
//! ## Workers
//!
//! Workers run in a pool (configurable via `dispatcher.worker_count`). Each worker:
//! 1. Leases the next due job from the queue
//! 2. Runs the job through the configured pipeline of checkers
//! 3. Acknowledges success or reschedules on failure
//!
//! ## Pipeline
//!
//! The `Pipeline` contains a sequence of checkers that run in order:
//! - **Hygiene checkers**: Title format, description, size, autolabels
//! - **Multi-review checker**: Parallel LLM reviews from multiple providers
//!
//! ## Trust Gate
//!
//! The `trust` module implements a permission-based trust system:
//! - Authors with `write`/`maintain`/`admin` permission are trusted
//! - Untrusted authors require `/barry approve` from a maintainer
//! - Approval is sticky via a bot comment marker

pub mod debounce;
pub mod run;
pub mod trust;
pub mod worker;
