//! Extract the `[package.metadata.symposium]` table from a crate `Cargo.toml`.
//!
//! The table uses the *same schema* as a `Symposium.toml` plugin manifest — a
//! crate can define its plugin inline in `Cargo.toml` instead of (or in
//! addition to) shipping a separate file.
//! [`CargoPm::load_plugin`](crate::pm::CargoPm::load_plugin) deserializes
//! whatever this returns into a plugin manifest and merges it with any
//! `Symposium.toml` the crate ships (see
//! [`merge_crate_manifest_outcome`](crate::plugins::merge_crate_manifest_outcome)).

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// A failure reading or parsing a crate manifest's optional plugin metadata.
#[derive(Debug, Error)]
pub enum MetadataError {
    /// The manifest could not be read as UTF-8 text.
    #[error("failed to read crate manifest")]
    ReadManifest {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The manifest was not valid TOML with the expected Cargo structure.
    #[error("failed to parse crate manifest")]
    ParseManifest {
        path: Option<PathBuf>,
        #[source]
        source: toml::de::Error,
    },
}

/// Read a crate `Cargo.toml` and return its `[package.metadata.symposium]`
/// table, if present. The table is returned verbatim; validation against the
/// plugin-manifest schema happens in `plugins`.
pub fn symposium_metadata(cargo_toml_path: &Path) -> Result<Option<toml::Table>, MetadataError> {
    let content =
        std::fs::read_to_string(cargo_toml_path).map_err(|source| MetadataError::ReadManifest {
            path: cargo_toml_path.to_path_buf(),
            source,
        })?;
    symposium_metadata_str(&content).map_err(|error| match error {
        MetadataError::ParseManifest { source, .. } => MetadataError::ParseManifest {
            path: Some(cargo_toml_path.to_path_buf()),
            source,
        },
        read_error @ MetadataError::ReadManifest { .. } => read_error,
    })
}

/// Extract `[package.metadata.symposium]` from a `Cargo.toml` string.
pub(crate) fn symposium_metadata_str(content: &str) -> Result<Option<toml::Table>, MetadataError> {
    let doc: CargoToml = toml::from_str(content)
        .map_err(|source| MetadataError::ParseManifest { path: None, source })?;
    Ok(doc
        .package
        .and_then(|p| p.metadata)
        .and_then(|m| m.symposium))
}

// --- serde types for the Cargo.toml structure we navigate ---

#[derive(Deserialize)]
struct CargoToml {
    package: Option<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    metadata: Option<PackageMetadata>,
    #[serde(flatten)]
    _rest: toml::Table,
}

#[derive(Deserialize)]
struct PackageMetadata {
    symposium: Option<toml::Table>,
    #[serde(flatten)]
    _rest: toml::Table,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn extract_missing_metadata() {
        let toml = indoc! {r#"
            [package]
            name = "my-crate"
            version = "0.1.0"
            edition = "2021"
        "#};
        assert!(symposium_metadata_str(toml).unwrap().is_none());
    }

    #[test]
    fn extract_symposium_table() {
        // The block is returned verbatim, in the plugin-manifest schema.
        let toml = indoc! {r#"
            [package]
            name = "my-crate"
            version = "0.1.0"
            edition = "2021"

            [[package.metadata.symposium.skills]]
            source.path = "guidance"

            [[package.metadata.symposium.plugins]]
            source.cargo = "other-crate"
        "#};
        let table = symposium_metadata_str(toml).unwrap().unwrap();
        assert!(table.contains_key("skills"));
        assert!(table.contains_key("plugins"));
    }

    #[test]
    fn extract_ignores_unrelated_metadata() {
        // Other `[package.metadata.*]` keys don't interfere.
        let toml = indoc! {r#"
            [package]
            name = "my-crate"
            version = "0.1.0"
            edition = "2021"

            [package.metadata.docs.rs]
            all-features = true
        "#};
        assert!(symposium_metadata_str(toml).unwrap().is_none());
    }

    #[test]
    fn read_manifest_preserves_metadata_and_optional_absence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Cargo.toml");
        std::fs::write(
            &path,
            "[package.metadata.symposium]\nname = 'inline-plugin'\n",
        )
        .unwrap();
        let table = symposium_metadata(&path).unwrap().unwrap();
        assert_eq!(table["name"].as_str(), Some("inline-plugin"));

        for content in [
            "[workspace]\nmembers = []\n",
            "[package]\nname = 'plain-crate'\n",
            "[package.metadata.other]\nvalue = true\n",
        ] {
            std::fs::write(&path, content).unwrap();
            assert!(symposium_metadata(&path).unwrap().is_none());
        }
    }

    #[test]
    fn missing_manifest_preserves_path_and_io_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-manifest-path-canary.toml");
        let error = symposium_metadata(&path).unwrap_err();

        let MetadataError::ReadManifest {
            path: actual,
            source,
        } = &error
        else {
            panic!("expected a manifest read failure");
        };
        assert_eq!(actual, &path);
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .is::<std::io::Error>()
        );
        assert_eq!(error.to_string(), "failed to read crate manifest");
    }

    #[test]
    fn malformed_manifest_string_preserves_source_without_displaying_content() {
        let content = "[package]\nname = 'private-manifest-content-canary' extra\n";
        let error = symposium_metadata_str(content).unwrap_err();

        let MetadataError::ParseManifest { path, source } = &error else {
            panic!("expected a manifest parse failure");
        };
        assert!(path.is_none());
        assert!(
            source
                .to_string()
                .contains("private-manifest-content-canary")
        );
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .is::<toml::de::Error>()
        );
        assert_eq!(error.to_string(), "failed to parse crate manifest");
    }

    #[test]
    fn malformed_manifest_file_preserves_path_and_parser_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private-manifest-path-canary.toml");
        let content = "[package]\nname = 'private-manifest-content-canary' extra\n";
        std::fs::write(&path, content).unwrap();
        let error = symposium_metadata(&path).unwrap_err();

        let MetadataError::ParseManifest {
            path: actual,
            source,
        } = &error
        else {
            panic!("expected a manifest parse failure");
        };
        assert_eq!(actual.as_ref(), Some(&path));
        assert!(
            source
                .to_string()
                .contains("private-manifest-content-canary")
        );
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .is::<toml::de::Error>()
        );
        assert_eq!(error.to_string(), "failed to parse crate manifest");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}
