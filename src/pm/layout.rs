//! Registry directory layout: how a registry source's directory tree maps to
//! plugin-bearing entries.
//!
//! A registry is a collection of *entries* — one per plugin or standalone
//! skill directory. Which directories constitute entries is packaging
//! convention and lives here, in the package-manager layer; *interpreting*
//! an entry's manifest (the TOML schema, predicates, gating) stays in
//! [`crate::plugins`].
//!
//! The flat layout defined here — every directory containing a
//! [`MANIFEST_FILE`] or [`SKILL_FILE`] is an entry, discovered recursively,
//! and a claimed directory is not recursed into — backs [`PathPm`](super::PathPm),
//! the single registry-instance PM. An entry declares which dependencies
//! activate it through its own manifest `depends-on`, evaluated when the
//! plugin is loaded; the layout carries no dependency information itself.

use std::io;
use std::path::{Path, PathBuf};

/// Plugin manifest filename that marks a directory as a plugin entry.
pub const MANIFEST_FILE: &str = "Symposium.toml";

/// Skill file that marks a directory as a standalone-skill entry.
pub const SKILL_FILE: &str = "SKILL.md";

/// What kind of entry a directory is.
#[derive(Debug)]
pub enum EntryKind {
    /// A plugin entry; carries the path to its `Symposium.toml`.
    Plugin(PathBuf),
    /// A standalone-skill entry; carries the path to its `SKILL.md`.
    Skill(PathBuf),
}

/// Classify a directory as an entry, or `None` when it is neither.
/// [`MANIFEST_FILE`] takes precedence over [`SKILL_FILE`].
pub fn classify(dir: &Path) -> Option<EntryKind> {
    let manifest = dir.join(MANIFEST_FILE);
    if manifest.is_file() {
        return Some(EntryKind::Plugin(manifest));
    }
    let skill_md = dir.join(SKILL_FILE);
    if skill_md.is_file() {
        return Some(EntryKind::Skill(skill_md));
    }
    None
}

/// One entry in a registry source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    /// The entry directory, relative to the source root.
    pub subpath: PathBuf,
}

/// A registry source cannot be enumerated without losing entries.
///
/// Display text contains the operation and path; filesystem sources remain
/// available through [`std::error::Error::source`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// A source root is itself a plugin instead of containing plugins.
    #[error("plugin source root contains Symposium.toml — it should contain subdirectories with plugins, not be a plugin itself: {}", root.display())]
    RootIsPlugin {
        /// The rejected source root.
        root: PathBuf,
    },
    /// A source root is itself a skill instead of containing skills.
    #[error("plugin source root contains SKILL.md — it should contain subdirectories with skills, not be a skill itself: {}", root.display())]
    RootIsSkill {
        /// The rejected source root.
        root: PathBuf,
    },
    /// A directory could not be listed.
    #[error("cannot read registry directory: {}", path.display())]
    ReadDirectory {
        /// The directory whose listing failed.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: io::Error,
    },
    /// A directory entry or its metadata could not be read.
    #[error("cannot read registry entry: {}", path.display())]
    ReadEntry {
        /// The entry path, or its directory when the entry name is unavailable.
        path: PathBuf,
        /// The filesystem failure.
        #[source]
        source: io::Error,
    },
}

/// Enumerate the entries in the flat-layout registry source rooted at `root`,
/// sorted by subpath. A missing root yields no entries; a root that is itself
/// an entry is an error (a source should *contain* plugins, not be one).
///
/// Legacy adapter retained through root 0.6. New callers should use
/// [`try_enumerate`] to inspect [`LayoutError`] directly.
///
/// # Errors
/// Returns the same errors as [`try_enumerate`], preserving their typed source.
pub fn enumerate(root: &Path) -> anyhow::Result<Vec<RegistryEntry>> {
    Ok(try_enumerate(root)?)
}

/// Enumerate and sort a flat-layout registry without accepting partial results.
/// A missing root yields no entries; linked directories are followed with a
/// guard against ancestor cycles. Claimed entries are not recursed into.
///
/// # Errors
/// Returns [`LayoutError`] when the root is itself an entry, a present marker
/// is not a regular file, or directory/entry metadata cannot be read. No
/// partial listing is returned.
pub fn try_enumerate(root: &Path) -> Result<Vec<RegistryEntry>, LayoutError> {
    let read = match std::fs::read_dir(root) {
        Ok(read) => read,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(LayoutError::ReadDirectory {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    enumerate_entries(root, read)
}

/// Enumerate a configured registry whose root must exist and remain readable.
pub(crate) fn try_enumerate_existing(root: &Path) -> Result<Vec<RegistryEntry>, LayoutError> {
    let read = std::fs::read_dir(root).map_err(|source| LayoutError::ReadDirectory {
        path: root.to_path_buf(),
        source,
    })?;
    enumerate_entries(root, read)
}

fn enumerate_entries(
    root: &Path,
    read: std::fs::ReadDir,
) -> Result<Vec<RegistryEntry>, LayoutError> {
    match try_classify(root)? {
        Some(EntryKind::Plugin(_)) => {
            return Err(LayoutError::RootIsPlugin {
                root: root.to_path_buf(),
            });
        }
        Some(EntryKind::Skill(_)) => {
            return Err(LayoutError::RootIsSkill {
                root: root.to_path_buf(),
            });
        }
        None => {}
    }
    let mut entries = Vec::new();
    let mut ancestors = crate::dir_walk::Ancestors::rooted_at(root);
    walk(root, Path::new(""), read, &mut ancestors, &mut entries)?;
    entries.sort_by(|a, b| a.subpath.cmp(&b.subpath));
    Ok(entries)
}

fn try_classify(dir: &Path) -> Result<Option<EntryKind>, LayoutError> {
    let is_file = |path: &Path| {
        let read_error = |source| LayoutError::ReadEntry {
            path: path.to_path_buf(),
            source,
        };
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(read_error(source)),
        };
        let metadata = if metadata.file_type().is_symlink() {
            std::fs::metadata(path).map_err(read_error)?
        } else {
            metadata
        };
        if !metadata.is_file() {
            return Err(read_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry entry marker must be a regular file",
            )));
        }
        Ok(true)
    };
    let manifest = dir.join(MANIFEST_FILE);
    if is_file(&manifest)? {
        return Ok(Some(EntryKind::Plugin(manifest)));
    }
    let skill_md = dir.join(SKILL_FILE);
    if is_file(&skill_md)? {
        return Ok(Some(EntryKind::Skill(skill_md)));
    }
    Ok(None)
}

/// Recursively collect entries, with subpaths relative to `rel`.
fn walk(
    dir: &Path,
    rel: &Path,
    read: std::fs::ReadDir,
    ancestors: &mut crate::dir_walk::Ancestors,
    entries: &mut Vec<RegistryEntry>,
) -> Result<(), LayoutError> {
    for entry in read {
        let entry = entry.map_err(|source| LayoutError::ReadEntry {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| LayoutError::ReadEntry {
            path: path.clone(),
            source,
        })?;
        let kind = crate::dir_walk::resolved_kind(&path, file_type).map_err(|source| {
            LayoutError::ReadEntry {
                path: path.clone(),
                source,
            }
        })?;
        if kind != Some(crate::dir_walk::EntryKind::Dir) {
            continue;
        }
        let sub = rel.join(entry.file_name());
        if try_classify(&path)?.is_some() {
            entries.push(RegistryEntry { subpath: sub });
        } else if ancestors.enter(&path) {
            let read = std::fs::read_dir(&path).map_err(|source| LayoutError::ReadDirectory {
                path: path.clone(),
                source,
            })?;
            let result = walk(&path, &sub, read, ancestors, entries);
            ancestors.leave();
            result?;
        }
    }
    Ok(())
}

/// A subpath as it appears in a package id: slash-separated, so ids are
/// stable across platforms.
pub(crate) fn subpath_key(subpath: &Path) -> String {
    subpath
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }

    #[test]
    fn flat_layout_finds_nested_entries_with_pruning() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("plug/Symposium.toml"));
        // Claimed as a plugin — the nested skill is not a separate entry.
        touch(&tmp.path().join("plug/inner/SKILL.md"));
        touch(&tmp.path().join("group/deep/skill/SKILL.md"));

        let entries = try_enumerate(tmp.path()).unwrap();
        let subpaths: Vec<_> = entries.iter().map(|e| e.subpath.clone()).collect();
        assert_eq!(
            subpaths,
            vec![PathBuf::from("group/deep/skill"), PathBuf::from("plug")]
        );

        // Missing root: no entries, no error.
        assert!(try_enumerate(&tmp.path().join("nope")).unwrap().is_empty());
    }

    /// A registry directory that links back into itself still enumerates.
    #[cfg(unix)]
    #[test]
    fn flat_layout_terminates_on_a_symlink_cycle() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("group/deep/skill/SKILL.md"));
        symlink("..", tmp.path().join("group/back")).unwrap();

        let entries = try_enumerate(tmp.path()).unwrap();
        let subpaths: Vec<_> = entries.iter().map(|e| e.subpath.clone()).collect();
        assert_eq!(subpaths, vec![PathBuf::from("group/deep/skill")]);
    }

    #[test]
    fn flat_layout_rejects_root_that_is_an_entry() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("SKILL.md"));
        assert!(matches!(
            try_enumerate(tmp.path()),
            Err(LayoutError::RootIsSkill { root }) if root == tmp.path()
        ));
        let err = enumerate(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("not be a skill itself"));
        assert!(matches!(
            err.downcast_ref::<LayoutError>(),
            Some(LayoutError::RootIsSkill { root }) if root == tmp.path()
        ));

        touch(&tmp.path().join("Symposium.toml"));
        assert!(matches!(
            try_enumerate(tmp.path()),
            Err(LayoutError::RootIsPlugin { root }) if root == tmp.path()
        ));
    }

    #[test]
    fn flat_layout_reports_directory_failure_with_its_source() {
        use std::error::Error;

        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-directory");
        touch(&file);

        let error = try_enumerate(&file).unwrap_err();
        assert!(matches!(
            &error,
            LayoutError::ReadDirectory { path, .. } if path == &file
        ));
        assert!(error.source().unwrap().is::<io::Error>());
    }

    #[test]
    fn configured_registry_requires_an_existing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");

        assert!(try_enumerate(&missing).unwrap().is_empty());
        assert!(matches!(
            try_enumerate_existing(&missing),
            Err(LayoutError::ReadDirectory { path, source })
                if path == missing && source.kind() == io::ErrorKind::NotFound
        ));
        touch(&missing.join("plugin/Symposium.toml"));
        assert_eq!(
            try_enumerate_existing(&missing).unwrap(),
            try_enumerate(&missing).unwrap()
        );
    }

    #[test]
    fn flat_layout_rejects_directory_markers() {
        for name in [MANIFEST_FILE, SKILL_FILE] {
            let tmp = tempfile::tempdir().unwrap();
            let marker = tmp.path().join("plugin").join(name);
            std::fs::create_dir_all(&marker).unwrap();

            assert!(matches!(
                try_enumerate(tmp.path()),
                Err(LayoutError::ReadEntry { path, source })
                    if path == marker && source.kind() == io::ErrorKind::InvalidData
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn flat_layout_rejects_broken_marker_links() {
        use std::os::unix::fs::symlink;

        for name in [MANIFEST_FILE, SKILL_FILE] {
            let tmp = tempfile::tempdir().unwrap();
            let plugin = tmp.path().join("plugin");
            std::fs::create_dir(&plugin).unwrap();
            let marker = plugin.join(name);
            symlink("missing-marker", &marker).unwrap();

            assert!(matches!(
                try_enumerate(tmp.path()),
                Err(LayoutError::ReadEntry { path, source })
                    if path == marker && source.kind() == io::ErrorKind::NotFound
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn flat_layout_follows_marker_links_to_files() {
        use std::os::unix::fs::symlink;

        for name in [MANIFEST_FILE, SKILL_FILE] {
            let tmp = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            let file = external.path().join("marker");
            touch(&file);
            let plugin = tmp.path().join("plugin");
            std::fs::create_dir(&plugin).unwrap();
            symlink(&file, plugin.join(name)).unwrap();

            assert_eq!(
                try_enumerate(tmp.path()).unwrap(),
                vec![RegistryEntry {
                    subpath: PathBuf::from("plugin"),
                }]
            );
        }
    }

    #[test]
    fn filesystem_error_display_does_not_include_source_text() {
        use std::error::Error;

        let canary = "LAYOUT_SOURCE_PRIVATE_CANARY";
        let path = PathBuf::from("registry");
        let errors = [
            LayoutError::ReadDirectory {
                path: path.clone(),
                source: io::Error::other(canary),
            },
            LayoutError::ReadEntry {
                path,
                source: io::Error::other(canary),
            },
        ];
        for error in errors {
            assert!(!error.to_string().contains(canary));
            let source = error.source().unwrap().downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.to_string(), canary);
        }
    }

    #[cfg(unix)]
    #[test]
    fn flat_layout_rejects_partial_listing_from_a_broken_link() {
        use std::error::Error;
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("plugin/Symposium.toml"));
        let broken = tmp.path().join("broken");
        symlink("missing", &broken).unwrap();

        let error = try_enumerate(tmp.path()).unwrap_err();
        assert!(matches!(
            &error,
            LayoutError::ReadEntry { path, source }
                if path == &broken && source.kind() == io::ErrorKind::NotFound
        ));
        assert!(error.source().unwrap().is::<io::Error>());
    }

    #[cfg(unix)]
    #[test]
    fn flat_layout_follows_external_directory_links() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        touch(&external.path().join("nested/SKILL.md"));
        symlink(external.path(), tmp.path().join("linked")).unwrap();

        assert_eq!(
            try_enumerate(tmp.path()).unwrap(),
            vec![RegistryEntry {
                subpath: PathBuf::from("linked/nested"),
            }]
        );
    }
}
