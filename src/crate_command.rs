//! Crate command: fetch crate sources by name and version.

use std::path::Path;

use crate::config::Symposium;
use crate::pm::CargoPm;

/// Result of dispatching a command.
pub enum DispatchResult {
    /// Successful output to display.
    Ok(String),
    /// Error message.
    Err(String),
}

/// Failure to resolve or fetch a crate source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CrateError {
    /// The complete registry set could not be resolved.
    #[error("could not resolve configured registries for crate lookup")]
    Registry(#[from] crate::config::RegistryError),
    /// The owning package manager could not fetch the requested source.
    #[error("failed to fetch crate source")]
    PackageManager(#[from] crate::pm::PackageManagerError),
}

/// Dispatch a crate lookup through the legacy display protocol, retained through root 0.6.
/// New callers should use [`try_dispatch_crate`] to retain structured errors.
pub async fn dispatch_crate(
    sym: &Symposium,
    name: &str,
    version: Option<&str>,
    cwd: &Path,
) -> DispatchResult {
    match try_dispatch_crate(sym, name, version, cwd).await {
        Ok(output) => DispatchResult::Ok(output),
        Err(error) => DispatchResult::Err(error.to_string()),
    }
}

/// Fetch a crate source and render its resolved identity and path.
///
/// # Errors
/// Returns [`CrateError`] when registry configuration is invalid or source
/// resolution/fetching fails.
pub async fn try_dispatch_crate(
    sym: &Symposium,
    name: &str,
    version: Option<&str>,
    cwd: &Path,
) -> Result<String, CrateError> {
    let deps = sym.workspace_deps(cwd);
    let id = CargoPm::id_for(name, version);
    let pms = sym.try_package_managers(&deps)?;
    let result = pms
        .try_fetch(&id, symposium_install::UpdateLevel::None)
        .await?;
    Ok(format!(
        "Crate: {}\nVersion: {}\nSource: {}\n",
        result.id.name,
        result.id.version,
        result.root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[tokio::test]
    async fn invalid_registry_preserves_source_before_any_fetch() {
        let temp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(temp.path()).unwrap();
        sym.config.defaults.symposium_recommendations = false;
        sym.config.defaults.user_plugins = false;
        sym.config.registries.push(crate::config::RegistryConfig {
            name: "invalid".into(),
            git: Some("https://synthetic-secret-canary.invalid/token".into()),
            path: Some("local".into()),
            auto_update: false,
        });
        let error = try_dispatch_crate(&sym, "unused", None, temp.path())
            .await
            .unwrap_err();
        assert!(matches!(&error, CrateError::Registry(_)));
        assert!(error.source().unwrap().is::<crate::config::RegistryError>());
        assert!(!error.to_string().contains("synthetic-secret-canary"));
    }
}
