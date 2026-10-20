# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Typed acquisition, refresh, permission and Git-cache APIs with structured
  `InstallError`/`GitError` sources and diagnostics that omit source URLs,
  command stderr and external response contents.
- A borrowed `AcquiredLocation` view and fallible executable/script resolution
  that validate public acquisition-result fields before using their paths.

### Fixed

- Reject missing global/Git Cargo executable hints and unsafe source/path
  components before acquisition effects. `validate_source_runnable` lets callers
  preflight selected managed scripts as well as executable hints before setup or
  refresh; no-source PATH/absolute-path policy stays with the caller. Reject
  option-like Cargo Git sources; `./-repository` remains a valid relative source.
  Unix executable preparation now
  distinguishes absent commands from metadata and permission failures.
- Preserve native operating-system paths in Cargo `--root` arguments, including
  the binstall fallback. Keep previous version caches until installation succeeds;
  optional cache maintenance and fallback failures have visible diagnostics.
- Prepare Git cache content and metadata before replacement, preserve the old
  cache when preparation or publication fails, and reject escaping/cyclic
  repository links while retaining internal links' copied contents.

### Compatibility

- Existing source structs, public fields, wire forms, acquisition signatures and
  infallible path methods remain available through the installer's independent
  0.1.x line. New callers use `try_*` APIs. Legacy infallible path/constructor
  assumptions are retained only until **symposium-install 0.2**, independently
  of the root application's release boundary; no package version is changed here.
- Missing optional cache metadata and unsuccessful freshness checks retain their
  documented cache-miss/stale-cache policies. Missing Unix command paths remain
  a no-op for executable preparation, preserving PATH-based commands.

## [0.1.1](https://github.com/symposium-dev/symposium/compare/symposium-install-v0.1.0...symposium-install-v0.1.1) - 2026-10-01

### Other

- release

## [0.1.0](https://github.com/symposium-dev/symposium/releases/tag/symposium-install-v0.1.0) - 2026-09-30

### Other

- Add auto-update logic for installations
- Add custom predicate extensions via [[predicate]]
- Add global install option for cargo and add env vars
- introduce GitSource enum and reduce git API surface
- extract symposium-install crate and make symposium-hook async
