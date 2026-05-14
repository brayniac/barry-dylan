//! Barry Dylan - Automated PR review for GitHub
//!
//! A GitHub App that runs automated PR review across one or a few organizations.
//! Single Rust binary, embedded SQLite, webhook-driven.
//!
//! # Overview
//!
//! Barry Dylan provides multi-reviewer LLM review for pull requests:
//! - Two visible reviewers (Barry and Other Barry) with different LLM providers/personas
//! - A hidden judge that decides whether they materially agree
//! - PR hygiene checks (title format, description, size warning, auto-labels)
//! - Trust gates for untrusted contributors
//!
//! # Architecture
//!
//! ## Core Components
//!
//! - **App Runtime** (`app_runtime`): Entry point, config loading, worker pool, HTTP server
//! - **Checker** (`checker`): PR checkers - hygiene and multi-review LLM
//! - **Config** (`config`): Configuration parsing and validation
//! - **Dispatcher** (`dispatcher`): Job queue, leasing, worker execution
//! - **GitHub** (`github`): GitHub API clients (GraphQL/REST)
//! - **LLM** (`llm`): LLM client abstractions (Anthropic, OpenAI)
//! - **Storage** (`storage`): SQLite actor with blocking thread
//! - **Telemetry** (`telemetry`): Tracing and metrics
//! - **Webhook** (`webhook`): Webhook event handling, verification
//!
//! ## Key Patterns
//!
//! ### Single-Actor SQLite
//!
//! All database access goes through a single blocking thread via message passing.
//! The `Store` struct holds a sender to the actor and a read cache.
//!
//! ### Job Queue
//!
//! Jobs are leased with a timeout, allowing multiple workers to process events
//! concurrently without duplicates. The `enqueue` operation coalesces pending
//! jobs for the same (repo, pr, event_kind).
//!
//! ### Multi-GitHub Factory
//!
//! The `MultiGhFactory` trait provides GitHub clients for different identities
//! (Barry, Other Barry, Other Other Barry) per installation.
//!
//! ### Pipeline Checkers
//!
//! Each checker implements the `Checker` trait with `name()`, `enabled()`, and
//! `run()` methods. Checkers produce `CheckerOutcome` with status, summary,
//! and optional inline comments.
//!
//! ### LLM Client Abstraction
//!
//! Unified interface for Anthropic and OpenAI with retry logic for transient
//! errors (connection errors and 5xx status codes).
//!
//! # Usage
//!
//! ```toml
//! # barry.toml
//! [server]
//! listen = "0.0.0.0:8181"
//!
//! [github.barry]
//! app_id = 1
//! private_key_path = "/path/to/key.pem"
//! webhook_secret_env = "BARRY_WEBHOOK_SECRET"
//!
//! [github.other_barry]
//! app_id = 2
//! private_key_path = "/path/to/ob.pem"
//!
//! [github.other_other_barry]
//! app_id = 3
//! private_key_path = "/path/to/oob.pem"
//!
//! [storage]
//! sqlite_path = "/path/to/barry.db"
//!
//! [dispatcher]
//! worker_count = 4
//!
//! [llm.barry]
//! provider = "anthropic"
//! endpoint = "https://api.anthropic.com"
//! model = "claude-opus-4-7"
//!
//! [llm.other_barry]
//! provider = "openai"
//! endpoint = "https://api.openai.com/v1"
//! model = "gpt-5"
//!
//! [llm.other_other_barry]
//! provider = "openai"
//! endpoint = "https://api.openai.com/v1"
//! model = "gpt-5"
//!
//! [llm.judge]
//! provider = "anthropic"
//! endpoint = "https://api.anthropic.com"
//! model = "claude-haiku-4-5-20251001"
//! ```
//!
//! ```bash
//! # Start the server
//! cargo run --release -- run --config barry.toml
//! ```

pub mod app_runtime;
pub mod checker;
pub mod config;
pub mod dispatcher;
pub mod github;
pub mod llm;
pub mod storage;
pub mod telemetry;
pub mod webhook;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;
