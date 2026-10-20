# Contributing to Symposium

Welcome! This section is for people who want to work on Symposium itself. If you're a crate author who wants to publish skills or hooks for your library, see [Supporting your crate](../crate-authors/supporting-your-crate.md) instead.

Read the [Rust contributor contract](./rust-practices.md) and
[common issues](./common-issues.md) before changing code.

## Building and testing

Symposium is a Cargo workspace containing the application, SDK, installer, test
harness, and `xtask`. Start with these local checks from the repository root:

```sh
cargo check --workspace --all-targets --locked
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test --workspace --locked --no-fail-fast
```

The [test guide](./running-tests.md) lists formatting, strict Clippy, standalone
feature checks, documentation checks, and isolated CLI smoke testing. Real-agent
tests require explicit opt-in; a successful local-only run does not qualify a
real agent.

Tests use snapshot assertions via `expect-test`. Inspect a changed snapshot and
understand why it changed before updating the focused test, for example:

```sh
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  UPDATE_EXPECT=1 cargo test --locked --test dispatch
```

Review the resulting diff. A volatile temporary path or platform difference
usually needs a normalization fix rather than a new expected snapshot.

## Logging and debugging

Symposium uses `tracing` for structured logging. Commands that initialize logging
write timestamped `symposium-YYYYMMDD-HHMMSS.log` files. Logs go to
`$SYMPOSIUM_HOME/logs` when that override is set, otherwise to
`$XDG_STATE_HOME/symposium/logs` when configured, or the config directory's `logs`
subdirectory (normally `~/.symposium/logs`).

The default level is `info`. Set `[logging] level = "debug"` in the isolated
configuration's `config.toml` for more detail. Keep diagnostics free of secrets
and unrestricted payload dumps; use synthetic inputs when reproducing sensitive
failures. Hook stdout belongs to the agent protocol.

## What to read next

- [Rust practices](./rust-practices.md) — implementation and review contract
- [Key repositories](./repositories.md) — the repos that make up Symposium
- [Key modules](./module-structure.md) — the main pieces of the codebase
- [Important flows](./important-flows.md) — key paths through the code
- [Running tests](./running-tests.md) — checks, isolation, and agent opt-in
- [Writing tests](./testing-guidelines.md) — fixtures and test modes
- [Governance](./governance.md) — how we work together
