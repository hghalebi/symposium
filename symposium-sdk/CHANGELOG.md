# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Add `PredicateSyntaxError` and canonical `try_*` parsers that retain concrete
  version-parser sources without echoing predicate input. Existing parser
  signatures remain SDK 0.2 compatibility adapters.
- Exhaustively classify the closed predicate alternatives in dependency
  introspection; preserve nested composition and deduplicated dependency names.

### Added

- Typed `HookHandler<E>`, `try_default_handle_event`, and `try_run` APIs preserve
  concrete handler, runtime, input, serialization, and response-delivery errors
  without requiring `Send`, `Sync`, or `'static` handlers.
- `env::try_var` reports dependency-record emission separately from environment
  reads, preserving the original I/O or `VarError` source.

### Fixed

- Context-free `PreToolUse` denials are emitted rather than mistaken for empty
  allowing responses. Response write and flush failures return typed errors.
- Predicate records propagate JSON serialization, write, and flush failures;
  filesystem helpers stop before reading when dependency delivery fails.
- The context-injection example ignores only absent optional notes; unreadable
  notes propagate their I/O failure.

### Compatibility

- SDK 0.1 handler implementations, the `anyhow` re-export, and the existing
  single-handler-type `run` and `default_handle_event` entry points remain
  supported until the independent SDK 0.2 migration boundary.
- `env::var` retains its `VarError` return type and reports watch-delivery failure
  with a fixed best-effort stderr warning before the historical environment read.
  The legacy signature cannot retain watch or diagnostic-delivery errors; use
  `env::try_var` for the complete failure contract.
- Harness and watcher displays omit input, environment values, and source text.
  Structured sources are retained for deliberate inspection and can contain
  private data. The synchronous harness rejects an already entered Tokio runtime
  instead of panicking.

## [0.1.1](https://github.com/symposium-dev/symposium/compare/symposium-sdk-v0.1.0...symposium-sdk-v0.1.1) - 2026-10-01

### Other

- release

## [0.1.0](https://github.com/symposium-dev/symposium/releases/tag/symposium-sdk-v0.1.0) - 2026-09-30

### Added

- *(sdk)* env::var and fs::read_to_string helpers
- *(predicate)* parse CustomPredicateEvent JSONL stream

### Other

- *(pm)* package managers return unvalidated plugins
- *(sdk)* the manifest schema and predicate syntax belong to the SDK
- *(crate-authors)* the hook handler guide names a crate that exists
- Introduce the package-manager layer and reshape plugin/skill resolution
- Load crates as plugins via [[plugins]] chained references
- Workspace plugins: the workspace's own dirs define plugins
- Merge pull request #252 from kurasaiteja/tejakura-custom-predicate-jsonl
- switch custom predicate stdout to JSONL
- Make WorkspaceCrate and LoadedWorkspace non-exhaustive
- Address PR review feedback
- Use symposium-sdk types directly instead of re-exporting
- Require SymposiumDirs when constructing WorkspaceDeps
- Replace ad-hoc WorkspaceDeps fields with SymposiumDirs
- Add SymposiumDirs to symposium-sdk for shared path resolution
- Add WorkspaceDeps with in-process and disk caching
- Introduce symposium-sdk crate and refactor main crate to use it
