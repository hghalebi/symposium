//! Shared installation resolution for hooks and subcommands.
//!
//! Validate the public installation fields before any acquisition or setup
//! effects, then resolve the validated runnable through the installer's typed API.

use std::path::PathBuf;

use thiserror::Error;

use crate::config::Symposium;
use crate::plugins::{Installation, RunnableSelectionError, RunnableSpec};
use symposium_install::{AcquiredLocation, Runnable, try_acquire_source, try_make_executable};

/// Installation failures retain structured causes without displaying command bodies.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstallationError {
    /// The public installation/override fields violate the runnable contract.
    #[error(transparent)]
    Selection(#[from] RunnableSelectionError),
    /// Source acquisition, resolution, or executable preparation failed.
    #[error(transparent)]
    Installer(#[from] symposium_install::InstallError),
    /// A post-install command could not be started; the source is retained.
    #[error("failed to start installation command {index}")]
    CommandIo {
        index: usize,
        #[source]
        source: std::io::Error,
    },
    /// A post-install command completed unsuccessfully.
    #[error("installation command {index} failed with {status}")]
    CommandExit {
        index: usize,
        status: std::process::ExitStatus,
    },
    /// A caller attempted to run a pure setup installation.
    #[error("installation has no executable or script")]
    MissingRunnable,
    /// An acquired location cannot represent the selected command form.
    #[error("acquired source does not support the selected runnable")]
    InvalidAcquiredRunnable,
}

/// Run post-install shell commands sequentially, stopping at the first failure.
/// Their stdout goes to stderr because hook stdout belongs to the agent protocol.
pub(crate) async fn run_install_commands(commands: &[String]) -> Result<(), InstallationError> {
    for (index, cmd) in commands.iter().enumerate() {
        let index = index + 1;
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::io::stderr())
            .status()
            .await
            .map_err(|source| InstallationError::CommandIo { index, source })?;
        if !status.success() {
            return Err(InstallationError::CommandExit { index, status });
        }
    }
    Ok(())
}

/// Installation snapshot used to wire hook environment variables and PATH.
#[derive(Clone, Debug)]
pub struct AcquiredInstallation {
    /// Installation name; sanitized before it becomes an environment key.
    pub name: String,
    /// Managed cache/clone directory; absent for no-source and global Cargo.
    pub base: Option<PathBuf>,
    /// Resolved command, or absent for pure setup or a source without a binary.
    pub runnable: Option<AcquiredRunnable>,
}

/// The concrete command form used by a dispatcher.
#[derive(Clone, Debug)]
pub enum AcquiredRunnable {
    /// Run a resolved path through the shell.
    ResolvedScript { path: PathBuf },
    /// Run a resolved executable path directly.
    ResolvedExec { path: PathBuf },
    /// Run a global Cargo binary by name through PATH without adding hook variables.
    GlobalExec { path: PathBuf },
}

/// Acquire and prepare an installation after validating all runnable fields.
///
/// Source and hook overrides participate in one exclusive choice. No-source
/// paths retain process-current-directory/PATH resolution; there is no plugin
/// directory rebasing or permission changes. Executable preparation is limited
/// to managed sources; `update` controls the existing source freshness policy.
pub(crate) async fn acquire_installation(
    sym: &Symposium,
    installation: &Installation,
    override_executable: Option<&str>,
    override_script: Option<&str>,
    update: symposium_install::UpdateLevel,
) -> Result<AcquiredInstallation, InstallationError> {
    let spec = installation.runnable_spec(override_executable, override_script)?;
    let (executable_hint, script_hint) = match spec {
        Some(RunnableSpec::Exec(name)) => (Some(name), None),
        Some(RunnableSpec::Script(name)) => (None, Some(name)),
        None => (None, None),
    };
    let acquired = match &installation.source {
        Some(source) => {
            symposium_install::validate_source_runnable(source, executable_hint, script_hint)?;
            Some(try_acquire_source(&sym.install_context(), source, executable_hint, update).await?)
        }
        None => None,
    };
    let (base, runnable) = match acquired.as_ref() {
        Some(source) => match source.try_location()? {
            AcquiredLocation::GlobalCargo { .. } => {
                let Some(RunnableSpec::Exec(name)) = spec else {
                    return Err(InstallationError::InvalidAcquiredRunnable);
                };
                (
                    None,
                    Some(AcquiredRunnable::GlobalExec {
                        path: source.try_executable_named(name)?,
                    }),
                )
            }
            AcquiredLocation::Managed { base, .. } => {
                let runnable = match spec {
                    Some(RunnableSpec::Exec(name)) => Some(AcquiredRunnable::ResolvedExec {
                        path: source.try_executable_named(name)?,
                    }),
                    Some(RunnableSpec::Script(name)) => Some(AcquiredRunnable::ResolvedScript {
                        path: source.try_script_named(name)?,
                    }),
                    None => source
                        .try_executable()?
                        .map(|path| AcquiredRunnable::ResolvedExec { path }),
                };
                (Some(base.to_path_buf()), runnable)
            }
            _ => return Err(InstallationError::InvalidAcquiredRunnable),
        },
        None => {
            let runnable = match spec {
                Some(RunnableSpec::Exec(name)) => Some(AcquiredRunnable::ResolvedExec {
                    path: PathBuf::from(name),
                }),
                Some(RunnableSpec::Script(name)) => Some(AcquiredRunnable::ResolvedScript {
                    path: PathBuf::from(name),
                }),
                None => None,
            };
            (None, runnable)
        }
    };
    run_install_commands(&installation.install_commands).await?;
    if base.is_some()
        && let Some(AcquiredRunnable::ResolvedExec { path }) = &runnable
    {
        try_make_executable(path)?;
    }
    Ok(AcquiredInstallation {
        name: installation.name.clone(),
        base,
        runnable,
    })
}

/// Refresh an already-acquired source without eagerly installing an absent one.
/// Runnable validation happens before refresh or setup effects. Setup commands
/// run only when the existing source was actually refreshed.
pub(crate) async fn refresh_installation_if_present(
    sym: &Symposium,
    installation: &Installation,
    override_executable: Option<&str>,
    override_script: Option<&str>,
) -> Result<(), InstallationError> {
    let spec = installation.runnable_spec(override_executable, override_script)?;
    let Some(source) = &installation.source else {
        return Ok(());
    };
    let (executable_hint, script_hint) = match spec {
        Some(RunnableSpec::Exec(name)) => (Some(name), None),
        Some(RunnableSpec::Script(name)) => (None, Some(name)),
        None => (None, None),
    };
    symposium_install::validate_source_runnable(source, executable_hint, script_hint)?;
    let refreshed = symposium_install::try_refresh_source_if_present(
        &sym.install_context(),
        source,
        executable_hint,
    )
    .await?;
    if refreshed {
        run_install_commands(&installation.install_commands).await?;
    }
    Ok(())
}

pub(crate) fn resolve_runnable(
    installation: AcquiredInstallation,
    _label: &str,
) -> Result<Runnable, InstallationError> {
    let runnable = installation
        .runnable
        .ok_or(InstallationError::MissingRunnable)?;
    Ok(match runnable {
        AcquiredRunnable::ResolvedScript { path } => Runnable::Script(path),
        AcquiredRunnable::ResolvedExec { path } | AcquiredRunnable::GlobalExec { path } => {
            Runnable::Exec(path)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use symposium_install::{CargoSource, Source, UpdateLevel};

    fn installation() -> Installation {
        Installation {
            name: "test".into(),
            requirements: vec![],
            install_commands: vec![],
            source: None,
            executable: None,
            script: None,
            args: vec![],
        }
    }

    fn shell_path(path: &std::path::Path) -> String {
        let path = path.to_string_lossy().replace('\\', "/");
        let path = if cfg!(windows) && path.as_bytes().get(1) == Some(&b':') {
            format!("/{}{rest}", path[..1].to_lowercase(), rest = &path[2..])
        } else {
            path
        };
        format!("'{}'", path.replace('\'', "'\\''"))
    }

    #[tokio::test]
    async fn invalid_runnable_fields_never_run_install_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let marker = tmp.path().join("must-not-run");
        for mask in 0u8..16 {
            if mask.count_ones() < 2 {
                continue;
            }
            let mut install = installation();
            install.executable = (mask & 1 != 0).then(|| "exec".into());
            install.script = (mask & 2 != 0).then(|| "script".into());
            install
                .install_commands
                .push(format!(": > {}", shell_path(&marker)));
            let error = acquire_installation(
                &sym,
                &install,
                (mask & 4 != 0).then_some("override-exec"),
                (mask & 8 != 0).then_some("override-script"),
                UpdateLevel::None,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, InstallationError::Selection(_)),
                "mask {mask}: {error}"
            );
            assert!(!marker.exists(), "mask {mask} ran setup before validation");
            assert!(matches!(
                refresh_installation_if_present(
                    &sym,
                    &install,
                    (mask & 4 != 0).then_some("override-exec"),
                    (mask & 8 != 0).then_some("override-script")
                )
                .await,
                Err(InstallationError::Selection(_))
            ));
            assert!(!marker.exists());
        }
    }

    #[tokio::test]
    async fn invalid_no_source_runnables_never_run_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let marker = tmp.path().join("must-not-run");
        for value in ["", "PRIVATE_RUNNABLE_CANARY\0tool"] {
            for field in 0..4 {
                let mut install = installation();
                install
                    .install_commands
                    .push(format!(": > {}", shell_path(&marker)));
                install.executable = (field == 0).then(|| value.into());
                install.script = (field == 1).then(|| value.into());
                let executable = (field == 2).then_some(value);
                let script = (field == 3).then_some(value);
                let error =
                    acquire_installation(&sym, &install, executable, script, UpdateLevel::None)
                        .await
                        .unwrap_err();
                assert!(matches!(
                    error,
                    InstallationError::Selection(RunnableSelectionError::InvalidRunnable { .. })
                ));
                assert!(!error.to_string().contains("PRIVATE_RUNNABLE_CANARY"));
                assert!(matches!(
                    refresh_installation_if_present(&sym, &install, executable, script).await,
                    Err(InstallationError::Selection(
                        RunnableSelectionError::InvalidRunnable { .. }
                    ))
                ));
                assert!(!marker.exists());
            }
        }
    }

    #[tokio::test]
    async fn invalid_managed_paths_are_rejected_before_acquisition_refresh_or_setup() {
        use symposium_install::{GithubSource, InstallError};
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("cargo-must-not-run"));
        std::fs::create_dir_all(sym.cache_dir()).unwrap();
        // Cache parents are files so a regression stops at local I/O instead of fetching.
        for cache in ["plugins", "binaries"] {
            std::fs::write(sym.cache_dir().join(cache), b"preserve").unwrap();
        }
        let marker = tmp.path().join("must-not-run");
        let sources = [
            Source::Cargo(CargoSource::new("test").with_version("1.0.0")),
            Source::Github(GithubSource::new("https://github.com/example/test")),
        ];
        for source in sources {
            for field in 0..4 {
                let mut install = installation();
                install.source = Some(source.clone());
                install
                    .install_commands
                    .push(format!(": > {}", shell_path(&marker)));
                let value = "../PRIVATE_RUNNABLE_CANARY";
                install.executable = (field == 0).then(|| value.into());
                install.script = (field == 1).then(|| value.into());
                let executable = (field == 2).then_some(value);
                let script = (field == 3).then_some(value);
                let error =
                    acquire_installation(&sym, &install, executable, script, UpdateLevel::None)
                        .await
                        .unwrap_err();
                assert!(matches!(
                    error,
                    InstallationError::Installer(InstallError::InvalidInput {
                        field: "executable or script"
                    })
                ));
                assert!(!error.to_string().contains("PRIVATE_RUNNABLE_CANARY"));
                assert!(matches!(
                    refresh_installation_if_present(&sym, &install, executable, script).await,
                    Err(InstallationError::Installer(InstallError::InvalidInput {
                        field: "executable or script"
                    }))
                ));
                assert!(!marker.exists());
                for cache in ["plugins", "binaries"] {
                    assert_eq!(
                        std::fs::read(sym.cache_dir().join(cache)).unwrap(),
                        b"preserve"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_cargo_modes_are_rejected_before_acquisition_or_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("cargo-must-not-run"));
        let marker = tmp.path().join("must-not-run");
        for global in [false, true] {
            for script in [None, Some("script.sh")] {
                let mut source = CargoSource::new("test");
                source.global = global;
                if !global {
                    source.git = Some("https://example.invalid/test".into());
                }
                let mut install = installation();
                install.source = Some(Source::Cargo(source));
                install.script = script.map(str::to_string);
                install
                    .install_commands
                    .push(format!(": > {}", shell_path(&marker)));
                assert!(matches!(
                    acquire_installation(&sym, &install, None, None, UpdateLevel::None).await,
                    Err(InstallationError::Selection(_))
                ));
                assert!(!marker.exists());
            }
        }
    }

    #[tokio::test]
    async fn setup_and_no_source_overrides_preserve_current_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let marker = tmp.path().join("setup-ran");
        let mut install = installation();
        install
            .install_commands
            .push(format!(": > {}", shell_path(&marker)));
        let setup = acquire_installation(&sym, &install, None, None, UpdateLevel::None)
            .await
            .unwrap();
        assert!(setup.runnable.is_none());
        assert!(setup.base.is_none());
        assert!(marker.exists());
        assert!(matches!(
            resolve_runnable(setup, "setup"),
            Err(InstallationError::MissingRunnable)
        ));
        install.install_commands.clear();
        let exec = acquire_installation(&sym, &install, Some("sh"), None, UpdateLevel::None)
            .await
            .unwrap();
        let runnable = resolve_runnable(exec, "PATH command").unwrap();
        let Runnable::Exec(path) = runnable else {
            panic!("expected exec");
        };
        assert_eq!(path, PathBuf::from("sh"));
        assert!(
            tokio::process::Command::new(path)
                .args(["-c", "exit 0"])
                .status()
                .await
                .unwrap()
                .success()
        );
        let script = acquire_installation(
            &sym,
            &install,
            None,
            Some("relative/script.sh"),
            UpdateLevel::None,
        )
        .await
        .unwrap();
        assert!(
            matches!(resolve_runnable(script, "script").unwrap(), Runnable::Script(path) if path == *"relative/script.sh")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_source_absolute_executables_preserve_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let executable = tmp.path().join("user-tool");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        for mode in [0o644, 0o700, 0o711, 0o555] {
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(mode)).unwrap();
            let mut install = installation();
            install.executable = Some(executable.to_str().unwrap().into());
            let acquired = acquire_installation(&sym, &install, None, None, UpdateLevel::None)
                .await
                .unwrap();
            assert!(acquired.base.is_none());
            let runnable = resolve_runnable(acquired, "user tool").unwrap();
            assert!(matches!(&runnable, Runnable::Exec(path) if path == &executable));
            assert_eq!(
                std::fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
                mode
            );
            if mode == 0o700 {
                assert!(runnable.spawn(&[] as &[&str]).unwrap().status.success());
            }
        }
    }

    #[tokio::test]
    async fn managed_cargo_exec_inference_and_script_use_existing_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("cargo-must-not-run"));
        let registry = sym.cache_dir().join("binaries/test");
        let base = registry.join("1.0.0");
        std::fs::create_dir_all(base.join("bin")).unwrap();
        let binary = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(base.join("bin").join(binary), b"cached binary").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                base.join("bin").join(binary),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        std::fs::write(registry.join("current"), "1.0.0").unwrap();
        for (executable, script) in [
            (None, None),
            (Some("tool"), None),
            (None, Some("scripts/tool.sh")),
        ] {
            let mut install = installation();
            install.source = Some(Source::Cargo(CargoSource::new("test")));
            install.executable = executable.map(str::to_string);
            install.script = script.map(str::to_string);
            let acquired = acquire_installation(&sym, &install, None, None, UpdateLevel::None)
                .await
                .unwrap();
            assert_eq!(acquired.base.as_ref(), Some(&base));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(base.join("bin").join(binary))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o744
                );
            }
            match resolve_runnable(acquired, "cached").unwrap() {
                Runnable::Exec(path) => assert_eq!(path, base.join("bin").join(binary)),
                Runnable::Script(path) => assert_eq!(path, base.join("bin/scripts/tool.sh")),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_source_paths_defer_filesystem_failures_to_spawn() {
        let tmp = tempfile::tempdir().unwrap();
        let sym = Symposium::try_from_dir(tmp.path()).unwrap();
        let parent = tmp.path().join("not-a-directory");
        std::fs::write(&parent, b"preserve").unwrap();
        let mut install = installation();
        install.executable = Some(parent.join("tool").to_str().unwrap().into());
        let acquired = acquire_installation(&sym, &install, None, None, UpdateLevel::None)
            .await
            .unwrap();
        let error = resolve_runnable(acquired, "user path")
            .unwrap()
            .spawn(&[] as &[&str])
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
        assert_eq!(std::fs::read(parent).unwrap(), b"preserve");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_executable_metadata_failure_preserves_the_io_source() {
        use std::error::Error as _;
        let tmp = tempfile::tempdir().unwrap();
        let mut sym = Symposium::try_from_dir(tmp.path()).unwrap();
        sym.set_cargo_override(tmp.path().join("cargo-must-not-run"));
        let registry = sym.cache_dir().join("binaries/test");
        let base = registry.join("1.0.0");
        let bin = base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("tool"), b"cached binary").unwrap();
        std::fs::write(registry.join("current"), "1.0.0").unwrap();
        let mut install = installation();
        install.source = Some(Source::Cargo(CargoSource::new("test")));
        install.executable = Some("tool".into());
        // Acquisition sees a complete cache. Setup then makes only the fixture's
        // bin path non-directory, exercising managed permission preparation.
        install.install_commands.push(format!(
            "mv {} {} && printf preserve > {}",
            shell_path(&bin),
            shell_path(&base.join("saved-bin")),
            shell_path(&bin),
        ));
        let error = acquire_installation(&sym, &install, None, None, UpdateLevel::None)
            .await
            .unwrap_err();
        assert!(matches!(error, InstallationError::Installer(_)));
        let mut source = error.source();
        let mut found_io = false;
        while let Some(error) = source {
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
                found_io = true;
            }
            source = error.source();
        }
        assert!(found_io);
        assert_eq!(std::fs::read(&bin).unwrap(), b"preserve");
        assert_eq!(
            std::fs::read(base.join("saved-bin/tool")).unwrap(),
            b"cached binary"
        );
    }

    #[tokio::test]
    async fn install_command_error_never_displays_command_contents() {
        let error = run_install_commands(&["# PRIVATE_COMMAND_CANARY\nexit 23".into()])
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InstallationError::CommandExit { index: 1, .. }
        ));
        assert!(!error.to_string().contains("PRIVATE_COMMAND_CANARY"));
    }
}
