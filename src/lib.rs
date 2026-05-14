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
