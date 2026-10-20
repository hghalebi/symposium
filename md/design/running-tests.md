# Running tests

## Local-only checks

From the repository root, explicitly remove both agent-testing variables from
child processes. The commands below use a POSIX shell (`sh`, Bash, or Zsh):

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test --workspace --locked --no-fail-fast
```

The suite uses temporary fixture directories and project-scoped hooks. Agent-only
test bodies do not run without the enable gate, so passing this suite proves local
behavior only. `--no-fail-fast` lets Cargo report failures in later test binaries.

Workspace feature unification can hide minimal-library failures. When changing
SDK or installer APIs or features, also run these standalone checks and library
tests:

```sh
cargo check -p symposium-sdk --lib --locked --no-default-features
cargo check -p symposium-sdk --lib --locked --no-default-features --features clap
cargo check -p symposium-install --lib --locked --no-default-features
cargo check -p symposium-install --lib --locked --no-default-features --features clap
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test -p symposium-sdk --lib --locked --no-default-features
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test -p symposium-sdk --lib --locked --no-default-features --features clap
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test -p symposium-install --lib --locked --no-default-features
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test -p symposium-install --lib --locked --no-default-features --features clap
```

Use supported feature combinations rather than assuming `--all-features` is valid.
Run focused checks first, then the required workspace suite. For documentation:

```sh
cargo xtask md-orphan-check
mdbook build
git diff --check
```

Book rendering requires the `mdbook` and `mdbook-mermaid` tools configured by
`book.toml`; report missing tools separately from source-check results. Do not
remove a preprocessor to make a local check pass.

## Isolate CLI smoke tests

Do not run `init` or `sync` against your actual home or working projects during
validation. Prefer existing fixture tests. For a local help smoke check, this
POSIX-shell example uses both an isolated project and a temporary Symposium home:

```sh
(
  symposium_manifest="$PWD/Cargo.toml"
  symposium_check_dir=$(mktemp -d) || exit 1
  trap 'rm -rf "$symposium_check_dir"' EXIT
  mkdir -p "$symposium_check_dir/home" "$symposium_check_dir/project" || exit 1
  cd "$symposium_check_dir/project" || exit 1
  SYMPOSIUM_HOME="$symposium_check_dir/home" \
    cargo run --locked --manifest-path "$symposium_manifest" --bin cargo-agents -- --help
)
```

Tests of configuration mutations must keep `hook-scope = "project"` and assert
that invalid input leaves the original file intact. See [Writing tests](./testing-guidelines.md).

## Real agents are opt-in

The harness returns no configured agents unless
`SYMPOSIUM_ENABLE_AGENT_TESTING` is set. Its value is not parsed as a Boolean:
`0` also enables agent testing. Without that gate, `SYMPOSIUM_TEST_AGENT` alone
cannot enable the fixture runner's agent passes.

Once enabled, `SYMPOSIUM_TEST_AGENT` selects one agent and overrides
`test-agents.toml`. Without an override, the harness reads that gitignored file
from the repository root; if it cannot read the file, it falls back to
`claude-sdk`. To choose agents explicitly:

```toml
# test-agents.toml; used only after explicit enablement
# Use `acpr --list` to inspect ACP registry agents.
test-agents = ["claude-sdk"]
```

`test-agents = []` disables configured agents when no explicit override is set;
it is useful defense in depth in CI. Keep the enable variable absent in local
checks and CI, since an explicit agent override takes precedence over the list.

| Name | Backend | Notes |
|------|---------|-------|
| `claude-sdk` | Claude Agent SDK (Python) | Requires `uv` + `ANTHROPIC_API_KEY` |
| `kiro-cli-acp` | Kiro CLI via ACP | Requires `kiro-cli` in PATH |
| Any name from `acpr --list` | ACP registry via `acpr` | Auto-downloaded |

Only when you intend to run a real agent, use both variables:

```sh
SYMPOSIUM_ENABLE_AGENT_TESTING=1 SYMPOSIUM_TEST_AGENT=kiro-cli-acp \
  cargo test --locked --test hook_agent
```

These tests can invoke external services, use configured credentials, and
consume provider quota. Report their results separately from local-only checks.
Never print credentials when diagnosing agent setup.

## Focused tests and debugging

For example, run a single integration binary locally:

```sh
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test --locked --test init_sync
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test --locked --test dispatch
```

Add `-- --nocapture` when you need test output. On failure, the fixture harness
preserves its temporary directory and prints the path to stderr. Inspect the
fixture before updating snapshots; see [common issues](./common-issues.md) for
path normalization and reporting flakes.

## Windows

CI runs the full test suite on `windows-latest` as part of the `test` matrix (see `.github/workflows/ci.yml`). To run the tests locally on Windows:

- Install Git for Windows and make sure `sh` is on `PATH`. Git ships it at `C:\Program Files\Git\usr\bin`. Several tests spawn `sh` to run script-based hooks and predicates, so a missing `sh` shows up as unrelated-looking hook failures.
- The repo's `.gitattributes` normalizes checked-out text files to LF. This keeps shebang'd fixtures and shell scripts runnable regardless of `core.autocrlf`.

The self-update tests run on Windows: `set_mock_cargo` runs the `#!/bin/sh` mock through `sh` via a one-line `.cmd` shim (production spawns the cargo override directly, so no production code changes). The two `auto_update_re_execs_*` tests are `#[ignore]`d on Windows (`#[cfg_attr(windows, ignore)]`): they overwrite the running binary with a shebang stand-in and re-exec into it, which needs Windows-native process replacement. They still compile on Windows, so they are skipped (not compiled out) and their helpers need no `#[cfg]`. Porting them to run on Windows is a tracked follow-up.
