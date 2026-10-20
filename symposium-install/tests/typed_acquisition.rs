use std::path::{Path, PathBuf};

use symposium_install::{
    AcquiredKind, AcquiredLocation, AcquiredSource, CargoSource, GithubSource, InstallContext,
    InstallError, Source, UpdateLevel, acquire_source, make_executable, try_acquire_source,
    try_refresh_source_if_present,
};

#[test]
fn legacy_fields_variants_and_path_signatures_remain_available() {
    let _: fn(&AcquiredSource) -> Option<PathBuf> = AcquiredSource::executable;
    let _: fn(&AcquiredSource, &str) -> PathBuf = AcquiredSource::executable_named;
    let _: fn(&AcquiredSource, &str) -> PathBuf = AcquiredSource::script_named;
    let _: fn(&Path) -> anyhow::Result<()> = make_executable;

    let sources = [
        Source::Cargo(CargoSource::new("tool").with_version("1.0.0")),
        Source::Github(GithubSource::new("https://github.com/example/tool")),
    ];
    assert!(matches!(sources[0], Source::Cargo(_)));
    assert!(matches!(sources[1], Source::Github(_)));

    for kind in [AcquiredKind::Cargo, AcquiredKind::Github] {
        let source = AcquiredSource {
            base: Some(PathBuf::from("managed")),
            resolved_executable: Some("tool".into()),
            kind,
        };
        assert_eq!(source.executable(), Some(source.executable_named("tool")));
        assert_eq!(
            source.try_executable().unwrap(),
            Some(source.executable_named("tool"))
        );
        assert_eq!(
            source.try_script_named("./run.sh").unwrap(),
            source.script_named("./run.sh")
        );
    }
}

#[test]
fn managed_locations_resolve_both_layouts_with_and_without_default_executables() {
    for kind in [AcquiredKind::Cargo, AcquiredKind::Github] {
        for name in [None, Some("tool")] {
            let source = AcquiredSource {
                base: Some(PathBuf::from("managed")),
                resolved_executable: name.map(str::to_owned),
                kind,
            };
            match source.try_location().unwrap() {
                AcquiredLocation::Managed {
                    base,
                    kind: actual_kind,
                    resolved_executable,
                } => {
                    assert_eq!(base, Path::new("managed"));
                    assert_eq!(actual_kind, kind);
                    assert_eq!(resolved_executable, name);
                }
                _ => panic!("managed source changed location kind"),
            }
            let layout = match kind {
                AcquiredKind::Cargo => PathBuf::from("managed").join("bin"),
                AcquiredKind::Github => PathBuf::from("managed"),
                _ => panic!("test requires a known source layout"),
            };
            let executable = match kind {
                AcquiredKind::Cargo if cfg!(windows) => "tool.exe",
                _ => "tool",
            };
            assert_eq!(
                source.try_executable_named("tool").unwrap(),
                layout.join(executable)
            );
            assert_eq!(
                source.try_script_named("./run.sh").unwrap(),
                layout.join("run.sh")
            );
            assert_eq!(source.try_executable().unwrap().is_some(), name.is_some());
        }
    }
}

#[test]
fn global_cargo_resolves_its_bare_executable_and_rejects_scripts() {
    let source = AcquiredSource {
        base: None,
        resolved_executable: Some("tool".into()),
        kind: AcquiredKind::Cargo,
    };
    match source.try_location().unwrap() {
        AcquiredLocation::GlobalCargo { executable } => assert_eq!(executable, "tool"),
        _ => panic!("global source changed location kind"),
    }
    assert_eq!(
        source.try_executable().unwrap(),
        Some(PathBuf::from("tool"))
    );
    assert_eq!(
        source.try_executable_named("other-tool").unwrap(),
        PathBuf::from("other-tool")
    );
    assert!(matches!(
        source.try_script_named("run.sh"),
        Err(InstallError::GlobalScript)
    ));
}

#[test]
fn invalid_public_acquired_shapes_are_rejected_without_panics() {
    for (kind, executable) in [
        (AcquiredKind::Cargo, None),
        (AcquiredKind::Github, None),
        (AcquiredKind::Github, Some("tool")),
    ] {
        let source = AcquiredSource {
            base: None,
            resolved_executable: executable.map(str::to_owned),
            kind,
        };
        assert!(matches!(
            source.try_location(),
            Err(InstallError::InvalidLocation { .. })
        ));
        assert!(source.try_executable().is_err());
        assert!(source.try_executable_named("tool").is_err());
        assert!(source.try_script_named("run.sh").is_err());
    }
}

#[tokio::test]
async fn missing_required_hints_fail_before_creating_a_cache_or_running_cargo() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = InstallContext::new(tmp.path().join("not-created"))
        .with_cargo_bin(tmp.path().join("cargo-must-not-run"));
    let mut global = CargoSource::new("tool");
    global.global = true;
    let sources = [
        Source::Cargo(global),
        Source::Cargo(CargoSource::new("tool").with_git("local-repository")),
    ];

    for source in sources {
        let error = try_acquire_source(&ctx, &source, None, UpdateLevel::None)
            .await
            .err()
            .expect("missing hint must be rejected");
        assert!(matches!(error, InstallError::MissingExecutable { .. }));
        assert!(!ctx.cache_dir().exists());

        let error = acquire_source(&ctx, &source, None, UpdateLevel::None)
            .await
            .err()
            .expect("legacy entry point must also reject a missing hint");
        assert!(matches!(
            error.downcast_ref::<InstallError>(),
            Some(InstallError::MissingExecutable { .. })
        ));
        assert!(!ctx.cache_dir().exists());
    }
}

#[tokio::test]
async fn refreshing_an_absent_source_does_not_install_it() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = InstallContext::new(tmp.path().join("not-created"))
        .with_cargo_bin(tmp.path().join("cargo-must-not-run"));
    let source = Source::Cargo(CargoSource::new("tool"));
    assert!(
        !try_refresh_source_if_present(&ctx, &source, Some("tool"))
            .await
            .unwrap()
    );
    assert!(!ctx.cache_dir().exists());
}

#[cfg(unix)]
mod unix {
    use std::error::Error;
    #[cfg(target_os = "linux")]
    use std::ffi::OsString;
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use symposium_install::try_make_executable;

    #[cfg(target_os = "linux")]
    async fn acquire_under_native_cache_path(binstall_fails: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let cargo = tmp.path().join("fake-cargo");
        fs::write(
            &cargo,
            format!(
                r#"#!/bin/sh
set -eu
operation=$1
shift
root=
while test "$#" -gt 0; do
    case "$1" in
        --root) shift; root=$1 ;;
    esac
    shift
done
test -n "$root" || exit 91
mkdir -p "$root/bin"
printf '%s\n' "$operation" >> "$root/invocations"
if test "$operation" = binstall && test {binstall_fails} = true; then
    exit 23
fi
: > "$root/bin/tool"
"#
            ),
        )
        .unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
        let native_cache = tmp.path().join(OsString::from_vec(b"cache-\xff".to_vec()));
        let ctx = InstallContext::new(native_cache.clone()).with_cargo_bin(cargo);
        let source = Source::Cargo(
            CargoSource::new("tool")
                .with_version("1.0.0")
                .with_git("file:///symposium-test-not-a-repository"),
        );
        let acquired = try_acquire_source(&ctx, &source, Some("tool"), UpdateLevel::None)
            .await
            .unwrap();
        let base = acquired.base.as_deref().unwrap();
        assert!(base.starts_with(&native_cache));
        assert!(base.to_str().is_none());
        assert_eq!(
            acquired.try_executable().unwrap(),
            Some(base.join("bin/tool"))
        );
        assert!(base.join("bin/tool").is_file());
        assert_eq!(
            fs::read_to_string(base.join("invocations")).unwrap(),
            if binstall_fails {
                "binstall\ninstall\n"
            } else {
                "binstall\n"
            }
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn binstall_preserves_non_utf8_cache_roots() {
        acquire_under_native_cache_path(false).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn binstall_fallback_preserves_non_utf8_cache_roots() {
        acquire_under_native_cache_path(true).await;
    }

    #[test]
    fn executable_metadata_failures_preserve_path_and_io_source() {
        let tmp = tempfile::tempdir().unwrap();
        let blocked = tmp.path().join("not-a-directory");
        fs::write(&blocked, b"preserve me").unwrap();
        let target = blocked.join("tool");
        let error = try_make_executable(&target).unwrap_err();
        match &error {
            InstallError::Io { path, .. } => assert_eq!(path, &target),
            _ => panic!("metadata failure must preserve its path"),
        }
        let io_error = error
            .source()
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .expect("metadata failure must retain its I/O source");
        assert_ne!(io_error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(fs::read(&blocked).unwrap(), b"preserve me");
    }

    #[test]
    fn executable_preparation_adds_owner_execute_and_preserves_bare_command_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let executable = tmp.path().join("tool");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
        try_make_executable(&executable).unwrap();
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let bare_command = Path::new("symposium-command-not-in-current-directory");
        assert!(!bare_command.exists());
        try_make_executable(bare_command).unwrap();
    }
}
