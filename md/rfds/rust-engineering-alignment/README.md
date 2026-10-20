# Align Symposium's Rust code and contributor guidance

## TL;DR

- Apply the fork owner's Rust engineering contract to Symposium's own code,
  contributor instructions, review guidance, and CI.
- Repair input and I/O boundaries first, then migrate library errors and
  representations in small working increments.
- Preserve plugin protocols, public compatibility promises, consent, fixture
  isolation, and the existing good newtype and trait designs.
- Implementation in this fork was requested on 2026-10-20. This document tracks
  one validated checkpoint per step; publishing releases and distributing this
  policy to other projects remain outside the implementation scope.

## Motivation

Baseline: `git@github.com:hghalebi/symposium.git`, commit
`028e2895c8200ff66355733a0d210def7b08670a`, inspected on 2026-10-20.
The fork was cloned over authenticated SSH without changing host trust.

The fork owner's global contract requires Cargo-first operations, strict error
propagation, semantic types, narrow trait bounds, and concrete business modeling.
The code already has useful examples of these practices, but several boundaries
still depend on assertions, discarded failures, or unvalidated combinations.

The scope is this repository's implementation and development process. Existing
Symposium functionality and upstream integration remain the product baseline.

## Change in a nutshell

For example, `Symposium::from_environment` currently returns `Self`, panics when
home discovery fails, and discards directory-creation failures. The proposed
constructor returns a typed failure with operation/path/source context. The
binary converts it to a concise diagnostic and a failure exit status without
writing diagnostic prose to hook protocol stdout.

Similarly, a registry's runtime source becomes one explicit `Git | Path`
alternative after raw TOML validation. The persisted format remains compatible;
invalid both/neither inputs receive a deliberate documented error policy.

The proposed contributor chapter is [Rust practices](./proposed-rust-practices.md).
During implementation it will become `md/design/rust-practices.md`; agent and
review instructions will point to that single portable source.

## Detailed plans

### Baseline and observations

- `cargo metadata --no-deps --format-version 1 --locked` and
  `cargo fmt --all -- --check` pass.
- `cargo check --workspace --all-targets --locked` passes on this Mac with
  Rust/Cargo 1.97.1. An initial offline attempt lacked cached `acpr`; the normal
  locked check resolved dependencies and passed without a lockfile edit.
- Standalone SDK and installer library checks pass in both minimal and optional
  `clap` configurations, using the four commands listed below.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` fails on two
  existing `drop_non_drop` diagnostics in
  `src/telemetry/schema/plugin_hook/update.rs:342` and `:634`, both in tests.
  They predate this planning-only diff and must be corrected before a strict gate.
- `cargo test --workspace --locked --no-fail-fast` passes: 1,016 tests passed,
  zero failed, zero ignored across the reported groups, with both agent-testing
  variables removed. This does not qualify real-agent behavior.
- `cargo xtask md-orphan-check`, local Markdown link/structure checks, and
  `git diff --check` pass. `mdbook build` is blocked by the missing local
  `mdbook-mermaid` preprocessor; no configuration was weakened to bypass it.

### Map the contract to concrete boundaries

| Area | Observed source | Intended change |
| --- | --- | --- |
| Startup and logging | `src/config.rs:429-433,548-564,696-699`; binary `:18,34,76` | Typed construction, directory, current-directory and subscriber failures; preserve error sources. |
| Config loading | `src/config.rs:730-740` | Distinguish missing config from unreadable input; retain or explicitly migrate documented malformed-config policy. |
| Agent config JSON | `src/agents/mod.rs:403-413,1020-1027` and sibling adapters | Validate root and field shapes before mutation; reject incompatible JSON without overwriting the original. |
| Hook diagnostics | `src/hook.rs:350,382-388`; `src/config.rs:591` | Replace unrestricted payload/config logging with allowlisted safe metadata; test synthetic secret canaries. |
| Hook I/O | `src/hook.rs:356-359,799`; `src/hook_schema.rs:104-105` | Typed stream/translation failures; retain per-agent protocol and denial behavior. |
| Installer | `symposium-install/src/lib.rs:334,455-458,705-758,776-781`; `src/installation.rs:87-140` | Preserve OS paths, validate public acquisition inputs, remove invalid executable/script/base combinations. |
| Registry types | `src/config.rs:293-357,384-400,648-671` | Raw persisted schema separated from validated runtime source alternatives. |
| SDK errors | `symposium-sdk/src/hook.rs:524-598`; `symposium-sdk/src/env.rs:17-19`, `symposium-sdk/src/fs.rs:18-21` | Typed harness/watch errors and an explicit versioned plugin-error contract. |
| Closed enum matches | `symposium-sdk/src/predicate/syntax.rs:77-134` | Replace wildcard dependency-introspection arms with explicit existing variants. |

These are source-confirmed findings. Boundary regression tests in the relevant
implementation steps must prove the runtime behavior before each fix is claimed.
Test helpers, examples, and `#[cfg(test)]` code are separate from production
panic inventories; optional configuration and intentional cache misses are
separate from silently discarded operational errors.

### Preserve the architecture that already works

Reuse the private validated names in `src/telemetry/schema/name.rs`, bounded
values in `schema/mod.rs`, and the identity-domain/anchor relationship in
`src/telemetry/identity.rs`. Keep secret identity keys free of formatting traits.

Keep the associated input/output types and runtime adapter in `src/hook_schema.rs`.
Runtime agent selection justifies its dynamic dispatch; the plan does not replace
it with a generic framework or require blanket `Clone + Send + Sync + 'static`.

Use ordinary Rust sums/products to represent real domain alternatives. Preserve
effect order in hook dispatch. Recursive JSON merging is not generally
associative or commutative: do not regroup or parallelize it based on an assumed
monoid. Test actual laws such as final denial, ordered context, and neutral empty
contributions where the event contract supports them.

### Compatibility and failure-policy decisions

Keep raw TOML, JSON wire formats, registry aliases, hook exit semantics, and
supported public SDK contracts stable during internal changes. Missing optional
files may remain optional; permission failures must not masquerade as absence.
Best-effort maintenance must expose a diagnostic or structured partial outcome.

The current configuration guide promises warnings/defaults for malformed TOML.
Record an explicit migration decision before making this fatal. Rejecting
ambiguous registry sources is a behavior change that needs tests and docs.

Public SDK handler return types and installer fields/signatures cannot be
replaced silently. Inventory callers, examples, Rustdoc and downstream fixtures;
choose an additive typed API or a documented next-version break per slice.
Retain compatibility adapters only for a real supported transition, with a
removal/version boundary. Do not publish the fork or retarget upstream release
metadata as a side effect of this engineering work.

### Contributor and review surfaces

- `AGENTS.md`: remove stale TypeScript/Swift build instructions and point to the
  portable Rust chapter, common issues, and actual checks.
- `CLAUDE.md`: retain the existing `@AGENTS.md` pointer.
- `.github/copilot-code-review-instructions.md`: add concise review checks;
  correct the `..` explanation and preserve supported compatibility/migration
  documentation rather than copying the whole policy.
- `.github/PULL_REQUEST_TEMPLATE.md`: add change/why/how/learning headings while
  preserving the existing AI disclosure and repository governance.
- `md/design/welcome.md`, `running-tests.md`, and `md/SUMMARY.md`: document real
  testing defaults, checks, and the Rust guide.
- `.cargo/config.toml`: search all callers, then remove `setup`/`ci` aliases
  whose packages no longer exist. Preserve working aliases.

## Frequently asked questions

### Does this add a rule-distribution feature?

The agreed scope is Symposium's own code and contributor guidance. Existing
skill/hook/plugin distribution behavior stays within its current contract.

### Will every string become a newtype or every function become generic?

Use types only where they prevent demonstrated value confusion or invalid
states. Retain useful existing traits and dynamic dispatch. No category-theory
dependency or new workspace crate is required by this plan.

### How will local tests avoid launching real agents?

`symposium-testlib/src/lib.rs:654` returns no agents unless
`SYMPOSIUM_ENABLE_AGENT_TESTING` is present. Any value, including `0`, enables
the gate. Once enabled, an explicit `SYMPOSIUM_TEST_AGENT` overrides the config;
the fallback without a config is `claude-sdk`. The current guide's Claude/Kiro
default claim is stale.

Unset both variables in local validation. Keep CI's `test-agents = []` as
defense in depth, and ensure the enable variable is absent there. Real-agent
qualification requires a separate explicit opt-in and is reported separately.

### How will checks avoid changing the user's agent configuration?

Use existing temporary fixture directories, `Symposium::from_dir`, project-scoped
hooks, and synthetic content. CLI smoke checks require a fresh temporary
`SYMPOSIUM_HOME` and isolated project. Do not run `cargo agents init` or `sync`
against the user's actual home, credentials, or working projects.

## Implementation plan and status

The steps below are proposed independent review increments. Preserve a working
tree and record focused checks, full applicable gates, and prediction versus
observation in each implementation PR. Update this plan as work lands.

### Step 1: Establish the portable contract and a truthful baseline

Status: completed locally; hosted checkpoint pending.

Add the Rust chapter and references listed above, correct test guidance and stale
aliases after caller search, and align the PR template. Fix the two existing
Clippy test diagnostics without adding lint allowances or changing assertions.

Acceptance: the guide has one source, local-only test commands match the harness,
all mdbook files are indexed, and workspace Clippy with warnings denied is green.
Capture any other baseline failures separately before changing runtime behavior.
Restore the documented local Mermaid toolchain before claiming a rendered book
build; tool installation is a separate implementation setup action.

- [x] Contributor, review, PR-template, alias, and test-guide updates.
- [x] Existing Clippy failures resolved and baseline evidence recorded.

Observed: workspace formatting, compilation, warnings-denied Clippy, local tests,
Markdown orphan checks, and rendered mdbook pass on 2026-10-20. The two existing
`drop_non_drop` sites now use immediate discard bindings without changing
assertions. Task-local `mdbook-mermaid` 0.17.1 supplies the existing preprocessor.
The original baseline findings above remain historical evidence.

### Step 2: Propagate startup and configuration failures

Introduce typed configuration/startup errors using `thiserror`. Update
`Symposium::from_environment`, directory resolution/creation, `logs_dir`, logging
initialization, and the CLI assembly caller together. Use fallible subscriber
initialization. Handle current-directory failure without `expect`.

Tests: missing config versus unreadable config, a file where a directory is
required, log destination failure, subscriber conflict, and hook stdout isolation.
Use deterministic filesystem conflicts rather than root-sensitive permission
assumptions. Update configuration loading and contributor diagnostics docs.

- [ ] Startup and all callers return/report contextual errors without panics.
- [ ] Existing supported configuration behavior is preserved or explicitly migrated.

### Step 3: Validate agent config mutations and safe diagnostics

Validate JSON shapes at the shared boundary and each agent-specific nested
container before editing. Preserve unknown valid settings and original bytes on
rejection. Replace unrestricted input/config/trace logging with safe metadata.

Tests: scalar/array roots, wrong hook/MCP container shapes, unchanged files after
failure, and synthetic secret canaries absent from logs and trace output. Reuse
registration and protocol tests; update common issues and relevant agent docs.

- [ ] All affected agent adapters reject malformed shapes without panic or data loss.
- [ ] Logs and optional traces have an explicit safe-content contract.

### Step 4: Make hook transport failures explicit

Use typed parse, event/input mismatch, translation, serialization, and stream
errors through the hook path. Handle stdin/stdout/stderr failures deliberately,
including plugin-input writes. Do not turn a failed deny path into an allow.

Tests: failing writers, malformed events, final denial, context order, no-contribution
identity, format routing, and existing per-agent deny/rewrite/exit behavior in
`tests/hook_stdout.rs`, `hook_decisions.rs`, `hook_context.rs`, and related suites.
Update hook-flow docs and active RFDs only where the implementation intersects.

- [ ] Hook errors preserve sources and protocol-owned stdout remains clean.
- [ ] Pure decisions and ordered I/O remain distinct and observable.

### Step 5: Validate registry and installation alternatives once

Keep raw config/manifests unchanged; introduce runtime `Git | Path` and
`Exec | Script` alternatives at validation/shared resolution boundaries. Preserve
pure setup, inferred binaries, overrides, requirements and environment wiring.
Validate standalone installer entry points, retain OS-native paths, and stop
discarding executable-permission failures.

Trace/update `plugins`, `installation`, hooks, subcommands, custom predicates,
configuration roundtrips, and public installer consumers together. Split registry
and installer PRs if their compatibility decisions are independent.

Tests: both/neither source, relative paths and roundtrips, missing global executable,
conflicting runnable fields, non-UTF-8 Unix cache paths using fake Cargo, and every
supported runnable mode. Document public API and invalid-input behavior changes.

- [ ] Runtime impossible states are removed without changing persisted valid forms.
- [ ] Installer library validation works without relying on its parent application.

### Step 6: Complete typed library and SDK error boundaries

Inventory `anyhow` across the root library, installer, SDK and test tooling; move
production library/domain failures to cohesive typed errors. Keep `anyhow` only
at executable assembly boundaries. Preserve structured sources and documented
best-effort/partial-success behavior rather than making every cache miss fatal.

Plan the SDK handler error contract and watch-emitter failure behavior together.
Update handler implementations, both SDK examples, crate Rustdoc, author guide,
changelogs and consumer fixtures. An associated error type is justified only if
it expresses actual plugin-defined variation with the minimum runtime bounds.

Tests: failing metadata emitters, runtime construction failure, library source
chains, SDK example/consumer compilation, standalone default and `clap` features,
and existing plugin protocol fixtures. State the public migration/version choice.

- [ ] Published library APIs have an explicit compatible migration or version boundary.
- [ ] Errors remain typed below application assembly; failures are not silently lost.

### Step 7: Finish enum, visibility, and production-path alignment

Enumerate closed `Predicate` variants in dependency-introspection helpers and
add table coverage for existing/nested variants. Preserve required fallbacks for
external non-exhaustive enums. Map library and integration-test callers before
tightening individual visibility boundaries.

Classify remaining production assertions and discarded results by responsibility;
reuse validated values, error returns, or narrower state representations. Include
the telemetry invariant assertions in this pass without weakening its privacy or
identity model. Keep test-only conveniences separate from production lint scopes.

Tests: predicate behavior, affected external/test consumers, remaining boundary
failure cases, and the supported workspace feature matrix. Update module docs.

- [ ] No remaining production panic/error-discard site is left unclassified or unresolved.
- [ ] Visibility and trait bounds reflect real callers; no speculative abstractions added.

### Step 8: Enforce the verified contract in CI

After debt is repaired, add workspace-wide locked checks, warnings-denied Clippy,
and explicit SDK/installer standalone default/`clap` coverage. Retain Ubuntu,
macOS, musl and Windows testing. Use source-aware lint scope for production
unwrap/expect/panic prevention without turning test helpers into production code
or masking existing failures with blanket allowances.

Keep real agents opt-in and verify tests cannot write global agent configuration.
Run mdbook/orphan validation on PRs using the existing book toolchain without
adding deployment authority. CI must exercise an actual typed boundary regression,
not merely verify that contributor prose contains policy words.

- [ ] All required checks pass locally and on the exact pushed commit when implemented.
- [ ] PR bodies, review gates, and docs reconciliation follow the global GitHub lifecycle.

### Validation commands and evidence

Baseline commands executed for this plan:

```sh
cargo metadata --no-deps --format-version 1 --locked
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
env -u SYMPOSIUM_ENABLE_AGENT_TESTING -u SYMPOSIUM_TEST_AGENT \
  cargo test --workspace --locked --no-fail-fast
```

The test runner used a child environment with those two variables removed,
equivalent to the command shown. Subsequent implementation gates also require
standalone SDK/installer minimal and `clap` checks; workspace feature unification
otherwise masks their minimal configurations. All four invocations below were
also run successfully for this baseline:

```sh
cargo check -p symposium-sdk --lib --locked --no-default-features
cargo check -p symposium-sdk --lib --locked --no-default-features --features clap
cargo check -p symposium-install --lib --locked --no-default-features
cargo check -p symposium-install --lib --locked --no-default-features --features clap
```

Retain equivalent library test coverage in CI. These standalone combinations
must be rechecked when the public error contract or feature definitions change.

For each new dependency, inspect it with `cargo search`/`cargo info` and add it
with package-scoped `cargo add`. Never manually edit `Cargo.lock`; keep updates
scoped. No dependencies were added for this planning change.

Prediction: compilation/formatting would establish a usable baseline, while
the stricter policy would expose concrete gaps beyond contributor prose.
Observed: compilation, formatting and the local tests pass; strict Clippy has two
existing test failures, and source/caller review identifies the staged boundary
changes above. Documentation source checks pass; rendering needs the local
Mermaid preprocessor. Runtime reproductions of the audited input failures belong
to the corresponding implementation steps.

### Plan delivery status

- [x] Clone the named fork through SSH and verify its origin and base commit.
- [x] Audit contributor/CI guidance, error boundaries, and type/trait models.
- [x] Draft this plan and proposed portable Rust contributor chapter.
- [x] Record local tests, Clippy failures, and documentation-check/toolchain results.
- [ ] Implementation steps 1–8; commits, pushes, hosted CI and release qualification.
