//! GitHub API clients and wrappers.
//!
//! # Overview
//!
//! Barry Dylan uses GitHub's REST and GraphQL APIs to:
//! - Fetch PR context (files, comments, reviews, metadata)
//! - Post check runs and review comments
//! - Manage permissions and installation tokens
//!
//! # Components
//!
//! ## `GitHub` Client
//!
//! The `GitHub` struct provides methods for all GitHub operations:
//! ```ignore
//! pub struct GitHub {
//!     http: reqwest::Client,
//!     base: String,                // GitHub API base URL
//!     token: String,               // Installation access token
//!     cache: PermissionCache,      // In-memory permission cache
//! }
//! ```
//!
//! Key methods:
//! - `fetch_pr_context()`: GraphQL query for PR + files + comments + reviews
//! - `list_pr_files()`: REST API for file list
//! - `post_check_run()`: Create/update check runs
//! - `post_review()`: Post a review on a PR
//! - `add_label()`: Add labels to a PR
//! - `author_permission()`: Get author's permission level
//!
//! ## GraphQL Queries
//!
//! The client uses a single GraphQL query to fetch PR context:
//! ```graphql
//! query($owner: String!, $repo: String!, $number: Int!) {
//!   repository(owner: $owner, name: $repo) {
//!     pullRequest(number: $number) {
//!       # PR fields...
//!     }
//!   }
//! }
//! ```
//!
//! Files are fetched separately via REST because the GraphQL files connection
//! doesn't expose unified-diff patches needed for LLM review.
//!
//! ## Permission Cache
//!
//! `PermissionCache` is an in-memory TTL cache for permission lookups:
//! - Keys: `(owner, repo, user)`
//! - Value: permission level (`read`, `write`, `maintain`, `admin`)
//! - TTL: 5 minutes
//!
//! ## Error Handling
//!
//! `GhError` variants:
//! - `Http(reqwest::Error)`: HTTP request errors
//! - `Api { status, body }`: GitHub API errors (4xx/5xx)
//! - `NotFound`: Resource not found
//!
//! # Installation Authentication
//!
//! Barry Dylan uses installation access tokens:
//! 1. Exchange JWT for installation token via `/app/installations/{id}/access_tokens`
//! 2. Cache token with expiration
//! 3. Use token for all GitHub API requests
//!
//! See `src/github/app.rs` for JWT generation and token caching.

pub mod app;
pub mod check_run;
pub mod client;
pub mod pr;
