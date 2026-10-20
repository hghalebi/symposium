//! Filesystem helpers that emit predicate cache events.
//!
//! Use these wrappers instead of [`std::fs`] from a custom predicate so that
//! reading a file also declares a dependency on its contents. Symposium
//! fingerprints the file (`mtime + size`) at read time and invalidates the
//! cached predicate result when the fingerprint changes.

use std::io::{self, Write};
use std::path::Path;

use crate::predicate::PredicateEmitter;

/// Read `path` to a string and emit a
/// [`CustomPredicateEvent::WatchFile`] on stdout so Symposium invalidates the
/// cached predicate result when the file changes.
///
/// # Errors
///
/// Returns the dependency serialization or output error without reading the
/// file if the watch record cannot be emitted. Otherwise returns the original
/// filesystem error if the file cannot be read as UTF-8.
///
/// [`CustomPredicateEvent::WatchFile`]: crate::predicate::CustomPredicateEvent::WatchFile
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    read_to_string_with_emitter(path.as_ref(), &mut PredicateEmitter::stdout())
}

fn read_to_string_with_emitter(
    path: &Path,
    emitter: &mut PredicateEmitter<impl Write>,
) -> io::Result<String> {
    emitter.watch_file(path.to_path_buf())?;
    std::fs::read_to_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::CustomPredicateEvent;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "watch delivery failed",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn declares_dependency_and_reads_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fs.rs");
        let mut output = Vec::new();
        let contents =
            read_to_string_with_emitter(&path, &mut PredicateEmitter::new(&mut output)).unwrap();

        assert_eq!(contents, std::fs::read_to_string(&path).unwrap());
        assert_eq!(
            serde_json::from_slice::<CustomPredicateEvent>(&output).unwrap(),
            CustomPredicateEvent::WatchFile(path)
        );
    }

    #[test]
    fn watch_failure_is_not_hidden_by_file_read_failure() {
        let error =
            read_to_string_with_emitter(Path::new(""), &mut PredicateEmitter::new(FailingWriter))
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "watch delivery failed");
    }

    #[test]
    fn missing_file_preserves_filesystem_error_after_emitting_watch() {
        let mut output = Vec::new();
        let error =
            read_to_string_with_emitter(Path::new(""), &mut PredicateEmitter::new(&mut output))
                .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(output, b"{\"watchFile\":\"\"}\n");
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_watch_path_preserves_serialization_source() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_vec(vec![0xff]));
        let mut output = Vec::new();
        let error = read_to_string_with_emitter(&path, &mut PredicateEmitter::new(&mut output))
            .unwrap_err();

        assert!(output.is_empty());
        assert!(
            error
                .get_ref()
                .unwrap()
                .downcast_ref::<serde_json::Error>()
                .is_some()
        );
    }
}
