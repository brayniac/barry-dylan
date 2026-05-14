//! Multi-identity, multi-persona LLM review checker.
//!
//! Three GitHub App identities (Barry, Other Barry, OOB) each driven by their
//! own LLM. See `docs/superpowers/specs/2026-05-13-multi-reviewer-design.md`.

pub mod identity;
pub mod judge;
pub mod persona;
pub mod review;
pub mod synthesis;
