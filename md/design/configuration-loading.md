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

## Logging and startup diagnostics

`Symposium::try_init_logging` returns `LoggingError` if it cannot create the
log directory, open the log file, or install the global subscriber. A subscriber
already installed by the embedding application is a reported conflict, rather
than a panic. The log file is opened before subscriber installation; a conflict
can therefore leave an empty log file.

The binary reports construction, current-directory, and logging failures on
stderr and returns a failure exit status. Startup diagnostics never use hook
stdout. Keep the report layer disabled for ordinary hooks, because agents own
their stdout protocol.

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
