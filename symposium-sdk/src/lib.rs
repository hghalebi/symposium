#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable
    )
)]

//! SDK for writing symposium plugins in Rust.
//!
//! This crate is the single dependency for plugin binaries — hooks, custom
//! predicates, and subcommands. It re-exports the hook handler SDK and adds
//! types for other plugin interfaces.
//!
//! # Hooks
//!
//! ```no_run
//! use std::convert::Infallible;
//! use std::io::Write as _;
//! use std::process::ExitCode;
//! use symposium_sdk::hook::{HookHandler, PreToolUseInput, PreToolUseOutput, try_run};
//!
//! struct MyHook;
//!
//! impl HookHandler<Infallible> for MyHook {
//!     async fn pre_tool_use(
//!         &self,
//!         _event: &PreToolUseInput,
//!     ) -> Result<PreToolUseOutput, Infallible> {
//!         Ok(PreToolUseOutput::default())
//!     }
//! }
//!
//! fn main() -> ExitCode {
//!     match try_run(MyHook) {
//!         Ok(exit) => exit,
//!         Err(error) => {
//!             if writeln!(std::io::stderr().lock(), "symposium-hook: {error}").is_err() {
//!                 return ExitCode::FAILURE;
//!             }
//!             ExitCode::FAILURE
//!         }
//!     }
//! }
//! ```
//!
//! [`hook::try_run`] preserves typed handler and harness errors. Its `Display`
//! messages omit input and error-source contents; inspect sources deliberately
//! because they may contain private input. Existing handlers using the default
//! `anyhow` error and [`hook::run`] remain supported until SDK 0.2.
//!
//! # Custom predicates
//!
//! A custom predicate binary receives its argument via CLI args and signals
//! pass/fail via exit code. It may also stream [`CustomPredicateEvent`]s on
//! stdout to describe the inputs whose changes should invalidate its cached
//! result. Prefer the [`env::try_var`] and [`fs::read_to_string`] helpers, which
//! report their inputs automatically and propagate failures to report them.
//!
//! [`CustomPredicateEvent`]: predicate::CustomPredicateEvent
//!
//! # Package managers
//!
//! [`manifest`] is the `Symposium.toml` schema and [`pm`] the package
//! identity. Together they are what crosses the package-manager boundary: a PM
//! answers with a [`pm::PackageId`], a content directory, and a
//! [`manifest::RawPluginManifest`] it parsed, translated, or synthesized.
//! Validating that manifest (defaults, dormancy, trust) is Symposium's job,
//! not the PM's.

pub mod env;
pub mod fs;
pub mod hook;
pub mod manifest;
pub mod pm;
pub mod predicate;
