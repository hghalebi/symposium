//! Fetch crate sources by running `cargo fetch` in a temporary dummy package.
//!
//! This module avoids hitting `crates.io` HTTP endpoints directly. Instead, it
//! creates a throwaway package that depends on the target crate, runs
//! `cargo fetch` to populate cargo's registry cache, and then reads
//! `cargo metadata` (or, for a binary-only crate, the probe's `Cargo.lock`) to
//! locate the extracted source directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use cargo_metadata::MetadataCommand;

use super::{CrateSourceError, FetchResult, normalize_crate_name};

const PROBE_PACKAGE: &str = "symposium-crate-probe";

/// Fetch a crate via a temporary dummy cargo package.
///
/// `version_req` may be any valid Cargo version requirement (e.g. `"=1.2.3"`,
/// `"^1.0"`, `"*"`).
pub async fn fetch_via_cargo(
    crate_name: &str,
    version_req: &str,
) -> Result<FetchResult, CrateSourceError> {
    let crate_name = crate_name.to_string();
    let version_req = version_req.to_string();

    // The work is blocking (subprocess + filesystem + metadata parsing), so run
    // it on the blocking pool to avoid stalling the async runtime.
    tokio::task::spawn_blocking(move || fetch_sync(&crate_name, &version_req))
        .await
        .map_err(|source| CrateSourceError::JoinProbe { source })?
}

fn fetch_sync(crate_name: &str, version_req: &str) -> Result<FetchResult, CrateSourceError> {
    tracing::debug!("fetching crate via cargo probe");

    let temp = tempfile::Builder::new()
        .prefix("symposium-crate-probe-")
        .tempdir()
        .map_err(|source| CrateSourceError::CreateProbe { source })?;

    write_dummy_package(temp.path(), crate_name, version_req)?;
    run_cargo_fetch(temp.path(), crate_name, version_req)?;
    match find_crate_in_metadata(temp.path(), crate_name)? {
        Some(found) => Ok(found),
        None => {
            let cargo_home = home::cargo_home()
                .map_err(|source| CrateSourceError::LocateCargoHome { source })?;
            find_crate_in_lockfile(temp.path(), crate_name, &cargo_home)
        }
    }
}

/// Write a minimal package at `dir` that depends on `crate_name = version_req`.
fn write_dummy_package(
    dir: &Path,
    crate_name: &str,
    version_req: &str,
) -> Result<(), CrateSourceError> {
    let source_dir = dir.join("src");
    std::fs::create_dir_all(&source_dir).map_err(|source| CrateSourceError::WriteProbe {
        operation: "create source directory",
        path: source_dir,
        source,
    })?;
    let lib_path = dir.join("src/lib.rs");
    std::fs::write(&lib_path, "").map_err(|source| CrateSourceError::WriteProbe {
        operation: "write library source",
        path: lib_path,
        source,
    })?;

    // The `[workspace]` table makes this package its own workspace root so
    // that cargo does not try to attach it to a parent workspace if the
    // tempdir happens to be under one.
    let cargo_toml = format!(
        r#"[package]
name = "{PROBE_PACKAGE}"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"

[dependencies]
{crate_name} = {{ version = "{version_req}" }}

[workspace]
"#
    );
    let manifest_path = dir.join("Cargo.toml");
    std::fs::write(&manifest_path, cargo_toml).map_err(|source| CrateSourceError::WriteProbe {
        operation: "write manifest",
        path: manifest_path,
        source,
    })?;
    Ok(())
}

/// Run `cargo fetch` against the manifest at `dir/Cargo.toml`.
fn run_cargo_fetch(
    dir: &Path,
    crate_name: &str,
    version_req: &str,
) -> Result<(), CrateSourceError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .arg("fetch")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .output()
        .map_err(|source| CrateSourceError::InvokeCargo {
            executable: cargo,
            source,
        })?;

    if !output.status.success() {
        return Err(CrateSourceError::CargoFetchFailed {
            crate_name: crate_name.to_owned(),
            version_req: version_req.to_owned(),
            status: output.status,
        });
    }
    Ok(())
}

/// Look up the package for `crate_name` in the metadata for `dir` and return
/// its source path.
fn find_crate_in_metadata(
    dir: &Path,
    crate_name: &str,
) -> Result<Option<FetchResult>, CrateSourceError> {
    let metadata = MetadataCommand::new()
        .current_dir(dir)
        .exec()
        .map_err(|source| CrateSourceError::CargoMetadata { source })?;

    // Cargo normalizes hyphens/underscores in crate names; match loosely so
    // that a user query of `serde-json` finds the `serde_json` package (or
    // vice-versa).
    let normalized = normalize_crate_name(crate_name);
    let Some(package) = metadata
        .packages
        .iter()
        .find(|p| normalize_crate_name(&p.name) == normalized)
    else {
        return Ok(None);
    };

    let manifest_path: PathBuf = package.manifest_path.clone().into();
    let path = manifest_path
        .parent()
        .ok_or_else(|| CrateSourceError::ManifestWithoutParent {
            path: manifest_path.clone(),
        })?
        .to_path_buf();

    Ok(Some(FetchResult {
        name: package.name.to_string(),
        version: package.version.to_string(),
        path,
    }))
}

/// Locate `crate_name` through the probe's `Cargo.lock` and cargo's extracted
/// registry sources.
///
/// Cargo leaves a dependency without a lib target (a binary-only crate) out of
/// `cargo metadata` entirely, but `cargo fetch` still locks and extracts it.
fn find_crate_in_lockfile(
    dir: &Path,
    crate_name: &str,
    cargo_home: &Path,
) -> Result<FetchResult, CrateSourceError> {
    let lockfile_path = dir.join("Cargo.lock");
    let lockfile = std::fs::read_to_string(&lockfile_path).map_err(|source| {
        CrateSourceError::ReadLockfile {
            path: lockfile_path.clone(),
            source,
        }
    })?;
    let lockfile: toml::Table =
        toml::from_str(&lockfile).map_err(|source| CrateSourceError::ParseLockfile {
            path: lockfile_path,
            source,
        })?;

    let packages = lockfile
        .get("package")
        .and_then(|packages| packages.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let (name, version) = locked_probe_dependency(packages, crate_name).ok_or_else(|| {
        CrateSourceError::CrateNotLocked {
            crate_name: crate_name.to_owned(),
        }
    })?;

    let registry_src = cargo_home.join("registry/src");
    let entries = std::fs::read_dir(&registry_src).map_err(|source| {
        CrateSourceError::ReadRegistrySources {
            path: registry_src.clone(),
            source,
        }
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| CrateSourceError::ReadRegistryEntry {
            path: registry_src.clone(),
            source,
        })?;
        let candidate = entry.path().join(format!("{name}-{version}"));
        if candidate.join(".cargo-ok").is_file() {
            return Ok(FetchResult {
                name: name.to_owned(),
                version: version.to_owned(),
                path: candidate,
            });
        }
    }

    Err(CrateSourceError::SourceNotFound {
        name: name.to_owned(),
        version: version.to_owned(),
        registry_root: registry_src,
    })
}

/// The `(name, version)` the probe package's dependency on `crate_name` locked
/// to.
///
/// Read through the probe's own `dependencies` entry rather than the first
/// package with a matching name: the target may also appear, at another
/// version, among its own dependencies. Cargo writes the entry as `name`, or
/// as `name version` when the name alone is ambiguous.
fn locked_probe_dependency<'a>(
    packages: &'a [toml::Value],
    crate_name: &str,
) -> Option<(&'a str, &'a str)> {
    let field = |package: &'a toml::Value, key: &str| package.get(key)?.as_str();
    let probe = packages
        .iter()
        .find(|package| field(package, "name") == Some(PROBE_PACKAGE))?;

    let normalized = normalize_crate_name(crate_name);
    let mut entry = probe
        .get("dependencies")?
        .as_array()?
        .iter()
        .filter_map(|dep| dep.as_str())
        .map(|dep| dep.split(' '))
        .find(|parts| {
            parts
                .clone()
                .next()
                .is_some_and(|name| normalize_crate_name(name) == normalized)
        })?;
    let name = entry.next()?;
    let version = match entry.next() {
        Some(version) => version,
        None => packages
            .iter()
            .find(|package| field(package, "name") == Some(name))
            .and_then(|package| field(package, "version"))?,
    };
    Some((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    fn write_lockfile(dir: &Path, body: &str) {
        std::fs::write(dir.join("Cargo.lock"), body).unwrap();
    }

    fn extracted_source(cargo_home: &Path, dir_name: &str) -> PathBuf {
        let source = cargo_home
            .join("registry/src/index.crates.io-1949cf8c6b5b557f")
            .join(dir_name);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(".cargo-ok"), "").unwrap();
        source
    }

    fn write_binary_lockfile(dir: &Path) {
        write_lockfile(
            dir,
            indoc::indoc! {r#"
                version = 4

                [[package]]
                name = "bin_only"
                version = "0.1.1"

                [[package]]
                name = "symposium-crate-probe"
                version = "0.0.0"
                dependencies = ["bin_only"]
            "#},
        );
    }

    #[test]
    fn probe_write_failure_preserves_operation_path_and_io_source() {
        let probe = tempfile::tempdir().unwrap();
        let source_dir = probe.path().join("src");
        std::fs::write(&source_dir, "existing file").unwrap();

        let error = write_dummy_package(probe.path(), "bin_only", "*").unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::WriteProbe { operation: "create source directory", path, .. }
                if path == &source_dir
        ));
        assert!(error.source().unwrap().is::<std::io::Error>());
        assert_eq!(
            std::fs::read_to_string(&source_dir).unwrap(),
            "existing file"
        );
    }

    #[test]
    fn missing_lockfile_preserves_path_and_io_source() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();

        let error =
            find_crate_in_lockfile(probe.path(), "bin_only", cargo_home.path()).unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::ReadLockfile { path, source }
                if path == &probe.path().join("Cargo.lock")
                    && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(error.source().unwrap().is::<std::io::Error>());
    }

    #[test]
    fn malformed_lockfile_preserves_parser_source_without_displaying_content() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        write_lockfile(probe.path(), "credential = \"LOCKFILE_CANARY\" invalid");

        let error =
            find_crate_in_lockfile(probe.path(), "bin_only", cargo_home.path()).unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::ParseLockfile { path, .. }
                if path == &probe.path().join("Cargo.lock")
        ));
        assert!(error.source().unwrap().is::<toml::de::Error>());
        assert!(!error.to_string().contains("LOCKFILE_CANARY"));
    }

    #[test]
    fn absent_locked_crate_is_a_semantic_error() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        write_binary_lockfile(probe.path());

        let error = find_crate_in_lockfile(probe.path(), "MISSING_CRATE_CANARY", cargo_home.path())
            .unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::CrateNotLocked { crate_name }
                if crate_name == "MISSING_CRATE_CANARY"
        ));
        assert!(error.source().is_none());
        assert!(!error.to_string().contains("MISSING_CRATE_CANARY"));
    }

    #[test]
    fn unreadable_registry_root_preserves_io_source() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        write_binary_lockfile(probe.path());

        let error =
            find_crate_in_lockfile(probe.path(), "bin_only", cargo_home.path()).unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::ReadRegistrySources { path, source }
                if path == &cargo_home.path().join("registry/src")
                    && source.kind() == std::io::ErrorKind::NotFound
        ));
        assert!(error.source().unwrap().is::<std::io::Error>());
    }

    #[test]
    fn missing_source_is_distinct_from_an_unreadable_registry() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        let registry_root = cargo_home.path().join("registry/src");
        std::fs::create_dir_all(&registry_root).unwrap();
        write_binary_lockfile(probe.path());

        let error =
            find_crate_in_lockfile(probe.path(), "bin_only", cargo_home.path()).unwrap_err();
        assert!(matches!(
            &error,
            CrateSourceError::SourceNotFound { name, version, registry_root: root }
                if name == "bin_only" && version == "0.1.1" && root == &registry_root
        ));
        assert!(error.source().is_none());
    }

    #[test]
    fn incomplete_extraction_is_not_a_source() {
        let probe = tempfile::tempdir().unwrap();
        let cargo_home = tempfile::tempdir().unwrap();
        write_binary_lockfile(probe.path());
        let source = extracted_source(cargo_home.path(), "bin_only-0.1.1");
        std::fs::remove_file(source.join(".cargo-ok")).unwrap();

        let error =
            find_crate_in_lockfile(probe.path(), "bin_only", cargo_home.path()).unwrap_err();
        assert!(matches!(error, CrateSourceError::SourceNotFound { .. }));
    }

    #[test]
    fn metadata_error_preserves_its_concrete_source_without_displaying_it() {
        let error = CrateSourceError::CargoMetadata {
            source: cargo_metadata::Error::Io(std::io::Error::other("METADATA_CANARY")),
        };

        let metadata_source = error.source().unwrap();
        assert!(metadata_source.is::<cargo_metadata::Error>());
        assert!(metadata_source.source().unwrap().is::<std::io::Error>());
        assert!(!error.to_string().contains("METADATA_CANARY"));
    }

    #[tokio::test]
    async fn task_failure_preserves_its_concrete_source() {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let error = CrateSourceError::JoinProbe {
            source: task.await.unwrap_err(),
        };

        assert!(matches!(&error, CrateSourceError::JoinProbe { source } if source.is_cancelled()));
        assert!(error.source().unwrap().is::<tokio::task::JoinError>());
    }

    #[test]
    fn lockfile_fallback_finds_a_binary_only_crate() {
        let probe = tempfile::tempdir().unwrap();
        write_lockfile(
            probe.path(),
            indoc::indoc! {r#"
                version = 4

                [[package]]
                name = "bin_only"
                version = "0.1.1"
                source = "registry+https://github.com/rust-lang/crates.io-index"

                [[package]]
                name = "symposium-crate-probe"
                version = "0.0.0"
                dependencies = ["bin_only"]
            "#},
        );
        let cargo_home = tempfile::tempdir().unwrap();
        let source = extracted_source(cargo_home.path(), "bin_only-0.1.1");

        let found = find_crate_in_lockfile(probe.path(), "bin-only", cargo_home.path()).unwrap();
        assert_eq!(found.name, "bin_only");
        assert_eq!(found.version, "0.1.1");
        assert_eq!(found.path, source);
    }

    #[test]
    fn lockfile_fallback_picks_the_version_the_probe_depends_on() {
        let probe = tempfile::tempdir().unwrap();
        write_lockfile(
            probe.path(),
            indoc::indoc! {r#"
                version = 4

                [[package]]
                name = "tool"
                version = "1.0.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"

                [[package]]
                name = "tool"
                version = "2.0.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"
                dependencies = ["tool 1.0.0"]

                [[package]]
                name = "symposium-crate-probe"
                version = "0.0.0"
                dependencies = ["tool 2.0.0"]
            "#},
        );
        let cargo_home = tempfile::tempdir().unwrap();
        extracted_source(cargo_home.path(), "tool-1.0.0");
        let source = extracted_source(cargo_home.path(), "tool-2.0.0");

        let found = find_crate_in_lockfile(probe.path(), "tool", cargo_home.path()).unwrap();
        assert_eq!(found.version, "2.0.0");
        assert_eq!(found.path, source);
    }
}
