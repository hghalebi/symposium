# Configuration loading

## Directory resolution

User-wide paths are resolved explicitly. Configuration uses `SYMPOSIUM_HOME`,
then `XDG_CONFIG_HOME/symposium`, then `~/.symposium`. Cache uses
`SYMPOSIUM_HOME/cache`, then `XDG_CACHE_HOME/symposium`, then
`<config-dir>/cache`. Logs use `SYMPOSIUM_HOME/logs`, then
`XDG_STATE_HOME/symposium/logs`, then `<config-dir>/logs`.
The `dirs` crate supplies home-directory discovery. The fallible APIs preserve
operating-system paths, including non-Unicode Unix paths.

See the [configuration reference](../reference/configuration.md#directory-resolution) for the full resolution table.

## Config loading

The user config (`~/.symposium/config.toml`) is loaded once at startup into the `Symposium` struct. The file is deserialized into `RawConfig`, then validated into the runtime `Config` used by the rest of the code. If the file is missing or empty, defaults are used. If parsing fails, a warning is printed and defaults are used.

The binary uses `Symposium::try_from_environment`. `Symposium::try_from_dir`
provides the same loading policy for an isolated root and does not create or
rewrite `config.toml`. Missing configuration is optional; unreadable existing
configuration is an error. A directory named `config.toml`, for example, must
not silently look like a first-run installation. Directory-creation and
configuration-read failures return `ConfigError` with the operation, path, and
original I/O error. Missing required home discovery returns `DirectoryError`.
Malformed TOML still warns and falls back to defaults without rewriting the
file; its diagnostic does not echo configuration contents.

## Registry resolution

Persisted `RegistryConfig` fields and the `[[plugin-source]]` alias remain
compatible. At each operation, the canonical `Symposium::try_registry_instances`
checks the current public configuration and converts each source to one Git or
Path alternative. Relative paths still resolve against the configuration
directory. `try_package_managers` and `try_detached_managers` use the complete
validated registry set; they do not return a partial set after a failure.

Both `git` and `path`, neither field, an invalid Git source, or a failed probe of
the built-in `plugins/` directory returns `RegistryError`. Only `NotFound` means
that optional built-in directory is absent. An existing broken symlink remains
a registry so its later read failure can inhibit cleanup. These checks do not
fetch a source or rewrite configuration. Diagnostics identify the registry or
filesystem path without echoing Git URLs.

Rejecting both source fields replaces the old implicit Git precedence for
maintained operations. This prevents invalid configuration from looking like a
smaller authoritative source set during stale-skill cleanup. The public
infallible registry and manager methods retain their previous precedence and
best-effort behavior until the root crate's 0.6 release; new callers must use
the typed `try_*` methods. The same applies to `GitPm::try_new` versus its legacy
`Option` constructor. Keep raw persistence compatibility separate from runtime
source validity, especially when public fields can be changed after startup.

## Logging and startup diagnostics

`Symposium::try_init_logging` returns `LoggingError` if it cannot create the
log directory, open the log file, or install the global subscriber. A subscriber
already installed by the embedding application is a reported conflict, rather
than a panic. The log file is opened before subscriber installation; a conflict
can therefore leave an empty log file.

The canonical startup path also checks warning writes and flushes. If stderr
cannot receive a malformed-config or unknown-log-level warning, it returns a
typed warning-delivery error rather than panicking or silently selecting defaults.
The legacy infallible APIs retain best-effort warnings until their removal boundary.

For a recognized, supported `PreToolUse`, construction, current-directory, or
logging failure produces a fixed native denial before the diagnostic is attempted.
Kiro denies through exit 2; JSON-based agents receive their own denial format and
exit 0 so they read it. Other startup failures return a failure exit status.
Diagnostic prose stays on stderr; hook stdout carries only the native protocol.
Keep the report layer disabled for ordinary hooks, because agents own stdout.
If response delivery itself fails, a nonzero status cannot prove that an agent
which ignores exit codes received the denial.

The infallible `Symposium::from_environment`, `Symposium::from_dir`,
`Symposium::init_logging`, and `SymposiumDirs::from_environment` signatures are
retained for supported callers until the root crate's 0.6 breaking release.
Their legacy panic and best-effort behavior is compatibility debt; new callers
must use the `try_*` APIs. Changing an infallible constructor to return `Result`
in place would break existing source code, so this transition adds a typed
boundary without inventing a fallback directory or terminating from the library.

The reusable distinction is **absence versus failed observation**: defaults are
correct when an optional file does not exist, but a failed read cannot establish
that the file is absent. The same rule protects registry cleanup and cache reads
from interpreting an I/O failure as an empty source.
