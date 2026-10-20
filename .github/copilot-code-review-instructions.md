# Code review instructions

## Rust contributor contract

Review against [Rust practices](../md/design/rust-practices.md), the single source
for this repository's Rust contract, and [common issues](../md/design/common-issues.md).
Focus comments on observable correctness, supported compatibility, and missing
validation. In particular, check:

- New or changed production paths propagate typed errors with sources and context;
  they do not panic, discard operational failures, or expose sensitive payloads.
- Boundary validation preserves user files on rejection, hook stdout stays protocol
  output, and tests use isolated fixtures instead of global agent configuration.
- Newtypes, enums, and narrow trait bounds encode real requirements. Closed enum
  matches are exhaustive; struct patterns omit `..` only when new fields must force
  review. A pattern containing `..` accepts added fields.
- Visibility serves real callers. Compatibility shims and re-exports remain only
  for supported consumers or versioning promises; public API changes include a
  deliberate migration decision.
- Comments and reference docs describe current behavior and invariants. Preserve
  useful decision rationale and required migration guidance in the right docs.
- Cargo performs supported dependency operations. Validation follows the supported
  package, target, and feature matrix; local, live-agent, and hosted CI evidence
  remain distinct.

## RFD compliance

Check whether the PR's changes relate to an active RFD (listed under "Accepted" in `md/SUMMARY.md`, with corresponding content in `md/rfds/`). Completed RFDs (under "Completed") are not active — ignore those.

If the PR does relate to an active RFD:

1. **Implementation plan updates**: Verify that the RFD's "Implementation plan and status" section is updated in this PR to reflect the work done (e.g., checking off items, adding new steps discovered during implementation).
2. **Design consistency**: Compare the PR's approach against the RFD's design. Flag deviations that aren't documented in the RFD's status section. Minor deviations are fine if noted; undocumented major departures should be called out.
3. **Scope alignment**: If the PR introduces changes outside the RFD's stated scope, note this — it may belong in a separate PR or the RFD scope may need updating.

If the PR does not relate to any active RFD, skip these checks.
