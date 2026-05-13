pub mod config;
pub mod storage;
pub mod telemetry;
pub mod github;
pub mod llm;
pub mod webhook;
pub mod dispatcher;
pub mod checker;
pub mod app_runtime;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;
