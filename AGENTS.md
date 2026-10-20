# Project

## Introduction

@md/introduction.md

## Contributor contract

Follow the portable Rust contributor contract for implementation and review:

@md/design/rust-practices.md

Before changing code, read the recurring failure patterns and actual checks:

@md/design/common-issues.md
@md/design/running-tests.md

Design documentation is tracked in the mdbook. Read the chapters relevant to the
change and update them when behavior changes:

@md/SUMMARY.md
@md/design/module-structure.md
@md/design/important-flows.md

Symposium is a Rust Cargo workspace. Validate affected packages and supported
feature combinations with the commands in the test guide. Keep real-agent tests opt-in and isolate CLI checks
from the user's home and agent configuration.

## Collaboration and governance

Preserve supported SDK, installer, configuration, and hook-protocol contracts.
Update an applicable active RFD's implementation status with each increment.
Follow the PR template, including AI disclosure and useful validation evidence.
Include a `Co-authored-by` trailer with the actual agent identifier (for example,
`Codex <codex@openai.com>`) in commits created with AI collaboration.
