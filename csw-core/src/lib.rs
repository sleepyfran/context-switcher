//! Core domain and logic for `csw`.
//!
//! This crate is intentionally display-agnostic: it owns the domain model,
//! configuration handling, filesystem layout, git interaction, and editor
//! spawning. The CLI crate builds user-facing output on top of these
//! primitives.

pub mod cmux;
pub mod config;
pub mod editor;
pub mod errors;
pub mod git;
pub mod hooks;
pub mod identity;
pub mod ops;
pub mod paths;
pub mod progress;
pub mod shell;
pub mod sidecar;

pub use config::{Config, RepoConfig};
pub use errors::CswError;
