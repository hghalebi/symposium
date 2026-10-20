//! Resolved symposium directory paths.
//!
//! Plugin binaries (hooks, predicates, subcommands) use [`SymposiumDirs`] to
//! locate cache directories and construct [`WorkspaceDeps`]
//! with the correct cargo override and disk-cache path.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::pm::WorkspaceDeps;

/// Failure to resolve the installation's directory paths.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DirectoryError {
    /// The operating system did not provide the user's home directory.
    #[error("could not determine home directory")]
    HomeUnavailable,
}

/// Resolved directory paths for the symposium installation.
///
/// Construct via [`try_from_environment()`](Self::try_from_environment) in plugin
/// binaries, or obtain from the main `Symposium` struct via `.dirs()`.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SymposiumDirs {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub cargo_override: Option<PathBuf>,
}

impl SymposiumDirs {
    /// Construct with explicit paths (used by the main crate's test constructor).
    pub fn new(config_dir: PathBuf, cache_dir: PathBuf, cargo_override: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            cache_dir,
            cargo_override,
        }
    }

    /// Resolve paths from environment variables.
    ///
    /// Resolution order for config dir:
    /// 1. `SYMPOSIUM_HOME` env var
    /// 2. `XDG_CONFIG_HOME/symposium`
    /// 3. `~/.symposium`
    ///
    /// Cache dir: `SYMPOSIUM_HOME/cache` → `XDG_CACHE_HOME/symposium` → `<config_dir>/cache`.
    ///
    /// Cargo override: `SYMPOSIUM_CARGO` env var.
    ///
    /// Legacy infallible entry point retained until the root crate's 0.6 release.
    /// New callers should use [`Self::try_from_environment`].
    ///
    /// # Panics
    /// Panics when no Unicode override or home directory is available.
    #[cfg_attr(
        not(test),
        expect(
            clippy::expect_used,
            reason = "legacy infallible directory resolution; removed in root 0.6"
        )
    )]
    pub fn from_environment() -> Self {
        let config_dir = if let Ok(home) = env::var("SYMPOSIUM_HOME") {
            PathBuf::from(home)
        } else if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("symposium")
        } else {
            dirs::home_dir()
                .expect("could not determine home directory")
                .join(".symposium")
        };
        let cache_dir = resolve_cache_dir(&config_dir);
        let cargo_override = env::var("SYMPOSIUM_CARGO").ok().map(PathBuf::from);

        Self {
            config_dir,
            cache_dir,
            cargo_override,
        }
    }

    /// Resolve paths from the environment without panicking on a missing home.
    ///
    /// Uses the same precedence as [`Self::from_environment`] and preserves
    /// non-Unicode operating-system paths.
    ///
    /// # Errors
    /// Returns [`DirectoryError::HomeUnavailable`] when neither a configuration
    /// override nor the user's home directory is available.
    pub fn try_from_environment() -> Result<Self, DirectoryError> {
        let config_dir = try_resolve_config_dir(
            env::var_os("SYMPOSIUM_HOME"),
            env::var_os("XDG_CONFIG_HOME"),
            dirs::home_dir(),
        )?;
        let cache_dir = if let Some(home) = env::var_os("SYMPOSIUM_HOME") {
            PathBuf::from(home).join("cache")
        } else if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
            PathBuf::from(xdg).join("symposium")
        } else {
            config_dir.join("cache")
        };
        let cargo_override = env::var_os("SYMPOSIUM_CARGO").map(PathBuf::from);
        Ok(Self::new(config_dir, cache_dir, cargo_override))
    }

    /// Create a [`WorkspaceDeps`] with disk caching enabled and the
    /// correct cargo override.
    pub fn workspace_deps(&self, cwd: &Path) -> WorkspaceDeps {
        WorkspaceDeps::new(cwd, self)
    }
}

fn try_resolve_config_dir(
    symposium_home: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf, DirectoryError> {
    if let Some(home) = symposium_home {
        Ok(PathBuf::from(home))
    } else if let Some(xdg) = xdg_config_home {
        Ok(PathBuf::from(xdg).join("symposium"))
    } else {
        Ok(home_dir
            .ok_or(DirectoryError::HomeUnavailable)?
            .join(".symposium"))
    }
}

fn resolve_cache_dir(config_dir: &Path) -> PathBuf {
    if let Ok(home) = env::var("SYMPOSIUM_HOME") {
        return PathBuf::from(home).join("cache");
    }
    if let Ok(xdg) = env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("symposium");
    }
    config_dir.join("cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_home_is_an_error_without_an_override() {
        assert!(matches!(
            try_resolve_config_dir(None, None, None),
            Err(DirectoryError::HomeUnavailable)
        ));
    }

    #[test]
    fn config_overrides_do_not_require_a_home_directory() {
        assert_eq!(
            try_resolve_config_dir(Some("explicit".into()), Some("xdg".into()), None).unwrap(),
            PathBuf::from("explicit")
        );
        assert_eq!(
            try_resolve_config_dir(None, Some("xdg".into()), None).unwrap(),
            PathBuf::from("xdg").join("symposium")
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_override_preserves_non_unicode_path() {
        use std::os::unix::ffi::OsStringExt;

        let path = OsString::from_vec(b"config-\xff".to_vec());
        assert_eq!(
            try_resolve_config_dir(Some(path.clone()), None, None).unwrap(),
            PathBuf::from(path)
        );
    }
}
