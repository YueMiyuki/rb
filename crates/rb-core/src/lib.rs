//! Reads Cargo.toml and Cargo.lock itself and runs rustc. Never invokes the cargo binary.

pub mod build;
pub mod build_script;
pub mod cfgexpr;
pub mod config;
pub mod depinfo;
pub mod exec;
pub mod features;
pub mod git;
pub mod invocation;
pub mod layout;
pub mod lints;
pub mod lockfile;
pub mod manifest;
pub mod plan;
pub mod profile;
pub mod registry;
pub mod resolve;
pub mod run;
pub mod shell;
pub mod srchash;
pub mod timings;
pub mod unit;
pub mod workspace;

pub use build::{AlreadyReported, BuildOptions};
