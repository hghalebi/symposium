# Rust practices

This is the contributor contract for Symposium's own Rust code, tests, and
review. It sets requirements for changes; existing migration work is tracked in
the [Rust engineering alignment plan](../rfds/rust-engineering-alignment/README.md).
Use the [test guide](./running-tests.md) for supported validation commands.

## Work from evidence

Read applicable instructions, affected callers, tests, manifests, CI and known
failure patterns. Predict the result before a material change, then compare it
with observed checks. Preserve unrelated changes and keep each increment small,
coherent, and reviewable. Update affected documentation and explain one useful
principle with concrete examples.

## Use Cargo for supported operations

Use `cargo search` and `cargo info` to choose dependencies; `cargo add` and
`cargo remove` to manage them; scoped `cargo update` and `cargo generate-lockfile`
to manage resolution. Preserve workspace package, dependency kind, registry,
source, version, features and default-feature policy. Never hand-edit `Cargo.lock`.

Use Cargo scaffolding, formatting, inspection, build, lint, test and documentation
commands where supported. Check installed help and repository aliases. Manual
Cargo-managed-file edits require a concrete unsupported operation; investigate
failed commands before choosing a fallback. Review automated diffs.

## Preserve error meaning

Use typed `thiserror` library/domain errors and `anyhow` only at executable
assembly boundaries. Preserve error sources and add actionable operation/path
context at system boundaries. Distinguish absent optional input from permission,
format and I/O failures. Make partial outcomes and best-effort failures visible.

Do not add production panics, `unwrap`, `expect`, `todo!` or `unimplemented!`.
Propagate errors or remove invalid states. Validate untrusted data once at the
responsible boundary before mutation or execution. Keep the original user file
intact on validation failure; hook stdout belongs to the agent protocol.

Never log, commit, print, or copy raw credentials, private keys, bearer tokens,
password-bearing URLs, or other secrets. Do not emit unrestricted
payload/configuration dumps. Test logging with synthetic canaries.
Use `SAFETY` comments only for actual `unsafe` code.

## Model business meaning in types

Use newtypes when equal representations have different meanings: identifiers,
units, validated values, or states. A type alias does not distinguish values.
Keep invariant-bearing fields private and validate with smart constructors or
`TryFrom`; reserve `From` for infallible conversion. Expose only operations and
trait implementations that preserve invariants; avoid `Deref` or mutable access
that bypasses validation. Raw local values need no wrapper.

Use structs for coexisting facts and enums for alternatives. For example, a
registry has attribution/settings and exactly one runtime source: Git or Path.
Keep the raw persisted format separate from that runtime invariant. Prefer enums
over interacting Booleans and preserve supported wire/configuration formats.

Use semantic names. Boolean names should read as questions, such as `is_enabled`,
`should_retry`, or `has_permission`.

Use exhaustive matches for closed domain enums. External non-exhaustive enums
still need fallbacks. Omit `..` from a struct pattern when new fields must force
review; use it deliberately when other fields do not affect the operation.

## Bound actual capabilities

Introduce traits or generics only at a real variation, integration or composition
boundary. Use argument-position `impl Trait` for simple abstraction and named
generics with `where` for explicit type relationships or multiple bounds.
Use narrow trait bounds for only the capabilities actually needed; do not add
`Clone`, `Send`, `Sync` or `'static` by habit.

Use associated types for implementation-selected related types and generics for
caller-selected types. Choose static or dynamic dispatch for actual runtime
needs. Preserve the existing typed hook adapter and identity-domain constraints
as examples of justified abstractions. Marker traits and blanket implementations
must enforce their promises.

## Compose business logic deliberately

Use category-theoretic products, sums and composition through ordinary Rust when
they clarify business behavior. Keep pure decisions separate from effects;
use `Option` for absence and `Result` for failure where appropriate.

State and test the algebraic laws the implementation relies on. Hook denial is
final and context accumulation is ordered; generic JSON merging is not assumed
associative or commutative. Preserve these observable semantics instead of adding
an abstract composition framework.

## Keep boundaries narrow

Keep dependency direction acyclic and conversions explicit between domain,
transport, persistence, and configuration types.

Default to private visibility; widen through `pub(super)`, `pub(crate)` and `pub`
only for real callers or supported APIs. Map consumers and integration tests
before changing exports. Preserve SDK versioning promises and required migration
adapters; remove unused scaffolding only when no supported contract needs it.

Describe current behavior, invariants and non-obvious trade-offs in comments and
Rustdoc. Keep decision rationale and migration instructions in appropriate design
docs, runbooks or changelogs. Public APIs need useful Rustdoc; add Examples,
Errors, Panics and Safety sections only when applicable.

## Validate observable behavior

Use deterministic, isolated, local-first tests with real parsers, filesystem
behavior, temporary directories and representative fixtures. Add regression
coverage for each confirmed bug. Avoid mocks that invent external behavior and
avoid production architecture created solely for mocking.

Use the repository-supported workspace/feature/platform matrix for formatting,
compilation, Clippy with warnings denied, tests and documentation checks.
Inspect standalone library feature combinations as well as unified workspace
builds. Record static, local runtime, live-agent and hosted-CI evidence separately.

Real agents are an explicit opt-in. For local tests, remove both
`SYMPOSIUM_ENABLE_AGENT_TESTING` and `SYMPOSIUM_TEST_AGENT` from the child
environment. Keep CLI smoke tests in a temporary `SYMPOSIUM_HOME` and isolated
project, with project-scoped hooks.

Keep functions focused, use semantic names, and choose loops/iterators for
clarity. Introduce async I/O only for an actual concurrency or latency need;
support performance claims with evidence before optimizing.
