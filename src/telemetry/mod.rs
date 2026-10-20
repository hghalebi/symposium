//! Opt-in usage telemetry: local event log.
//!
//! Telemetry is **opt-in** (`[telemetry] enabled = true`), **per-user**, and
//! **local-first**: events are appended as JSON lines under
//! `<config-dir>/telemetry/`, one file per UTC day. Nothing is uploaded — that
//! is a separate step the user takes deliberately (e.g. `cargo agents telemetry
//! show`).
//!
//! Older daily files are rolled off after [`RETENTION_DAYS`].
//!
//! Every entry point here is best-effort: a failure to read or write the log
//! must never break a hook, so errors are logged and swallowed.

#[cfg(not(any(
    target_pointer_width = "16",
    target_pointer_width = "32",
    target_pointer_width = "64"
)))]
compile_error!("telemetry identity framing requires a 16-, 32-, or 64-bit pointer width");

mod identity;
mod schema;
mod state;
mod storage;

/// Namespace reserved for temporary files owned by telemetry storage.
///
/// Lazy cleanup may delete every file carrying this prefix, and status excludes
/// the same files from its stored-file and byte counts.
const TEMPORARY_FILE_PREFIX: &str = ".telemetry-tmp-";

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Daily event files older than this are deleted on roll-off.
pub const RETENTION_DAYS: i64 = 30;

/// A single telemetry event: a timestamp plus a kind-tagged payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEvent {
    /// When the event occurred (UTC).
    pub at: DateTime<Utc>,
    /// The kind-tagged payload. Flattened so a line reads
    /// `{"at": "...", "kind": "tool_use", ...}`.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The kind of a telemetry event and its associated data.
///
/// Anonymous by construction: no prompt text, command lines, or file paths are
/// recorded — only counts and coarse metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// An agent session began.
    SessionStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        agent: String,
        /// Plugins applicable to the workspace at session start.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        plugins: Vec<String>,
    },
    /// The user submitted a prompt.
    UserPrompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// The agent invoked a tool (named, but with no arguments captured).
    ToolUse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        tool: String,
    },
}

impl TelemetryEvent {
    /// Build an event stamped at the current time.
    pub fn now(kind: EventKind) -> Self {
        Self {
            at: Utc::now(),
            kind,
        }
    }

    /// Short kind name, for summaries.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            EventKind::SessionStart { .. } => "session_start",
            EventKind::UserPrompt { .. } => "user_prompt",
            EventKind::ToolUse { .. } => "tool_use",
        }
    }
}

/// Directory holding the per-day JSONL event files.
pub fn telemetry_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("telemetry")
}

fn file_for(dir: &Path, at: DateTime<Utc>) -> PathBuf {
    dir.join(format!("events-{}.jsonl", at.format("%Y-%m-%d")))
}

/// Append one event to today's log file. Best-effort.
pub fn record(config_dir: &Path, event: &TelemetryEvent) {
    let dir = telemetry_dir(config_dir);
    if let Err(error) = fs::create_dir_all(&dir) {
        tracing::warn!(operation = "create_directory", category = ?error.kind(), "telemetry: event not recorded");
        return;
    }
    let line = match serde_json::to_string(event) {
        Ok(mut s) => {
            s.push('\n');
            s
        }
        Err(error) => {
            tracing::warn!(operation = "serialize_event", category = ?error.classify(), "telemetry: event not recorded");
            return;
        }
    };
    let path = file_for(&dir, event.at);
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(error) = f.write_all(line.as_bytes()) {
                tracing::warn!(operation = "append_event", category = ?error.kind(), "telemetry: event not recorded completely");
            }
        }
        Err(error) => {
            tracing::warn!(operation = "open_event", category = ?error.kind(), "telemetry: event not recorded")
        }
    }
}

/// Convenience: stamp `kind` with the current time and record it.
pub fn record_kind(config_dir: &Path, kind: EventKind) {
    record(config_dir, &TelemetryEvent::now(kind));
}

/// Parse the date out of an `events-YYYY-MM-DD.jsonl` filename.
fn file_date(path: &Path) -> Option<NaiveDate> {
    let stem = path.file_name()?.to_str()?;
    let date = stem.strip_prefix("events-")?.strip_suffix(".jsonl")?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

#[derive(Debug, thiserror::Error)]
enum RetentionError {
    #[error("negative_retention")]
    NegativeDays,
    #[error("retention_duration_out_of_range")]
    DurationOutOfRange,
    #[error("retention_cutoff_out_of_range")]
    CutoffOutOfRange,
}

fn retention_cutoff(today: NaiveDate, retention_days: i64) -> Result<NaiveDate, RetentionError> {
    if retention_days < 0 {
        return Err(RetentionError::NegativeDays);
    }
    let duration =
        chrono::Duration::try_days(retention_days).ok_or(RetentionError::DurationOutOfRange)?;
    today
        .checked_sub_signed(duration)
        .ok_or(RetentionError::CutoffOutOfRange)
}

/// Delete daily files older than `retention_days`. Best-effort.
///
/// Negative or unrepresentable retention ranges leave every file untouched.
pub fn roll_off(config_dir: &Path, retention_days: i64) {
    if let Err(error) = roll_off_at(config_dir, retention_days, Utc::now().date_naive()) {
        tracing::warn!(operation = "retention", category = %error, "telemetry: retention skipped");
    }
}

fn roll_off_at(
    config_dir: &Path,
    retention_days: i64,
    today: NaiveDate,
) -> Result<(), RetentionError> {
    let cutoff = retention_cutoff(today, retention_days)?;
    let dir = telemetry_dir(config_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(operation = "read_retention_directory", category = ?error.kind(), "telemetry: retention incomplete");
            return Ok(());
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(operation = "read_retention_entry", category = ?error.kind(), "telemetry: retention incomplete");
                continue;
            }
        };
        let path = entry.path();
        if let Some(date) = file_date(&path)
            && date < cutoff
            && let Err(error) = fs::remove_file(&path)
        {
            tracing::warn!(operation = "remove_retained_file", category = ?error.kind(), "telemetry: retention incomplete");
        }
    }
    Ok(())
}

/// All event files, sorted by date (oldest first).
fn event_files(config_dir: &Path, operation: &'static str) -> Vec<PathBuf> {
    let dir = telemetry_dir(config_dir);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(operation, category = ?error.kind(), "telemetry: file listing incomplete");
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if file_date(&path).is_some() {
                    files.push(path);
                }
            }
            Err(error) => {
                tracing::warn!(operation, category = ?error.kind(), "telemetry: file listing incomplete");
            }
        }
    }
    files.sort_by_key(|p| file_date(p));
    files
}

/// Read all recorded events, oldest first.
///
/// Unreadable files and invalid event lines are skipped with safe warnings.
pub fn read_events(config_dir: &Path) -> Vec<TelemetryEvent> {
    let mut events = Vec::new();
    for path in event_files(config_dir, "list_events") {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) => {
                tracing::warn!(operation = "read_events", category = ?error.kind(), "telemetry: partial event read");
                continue;
            }
        };
        for (index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TelemetryEvent>(line) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::warn!(operation = "parse_event", category = ?error.classify(), line = index + 1, "telemetry: partial event read");
                }
            }
        }
    }
    events
}

/// The most recent `limit` events, oldest first within the returned slice.
pub fn recent_events(config_dir: &Path, limit: usize) -> Vec<TelemetryEvent> {
    let mut events = read_events(config_dir);
    if events.len() > limit {
        events.drain(0..events.len() - limit);
    }
    events
}

/// How much telemetry is stored locally — for `telemetry status`.
#[derive(Debug, Default)]
pub struct Usage {
    pub files: usize,
    pub events: usize,
    pub bytes: u64,
    pub oldest: Option<NaiveDate>,
    pub newest: Option<NaiveDate>,
}

/// Summarize what is stored on disk.
///
/// Counts nonblank event-file lines, including invalid events. Read failures
/// produce safe warnings and a partial summary.
pub fn usage(config_dir: &Path) -> Usage {
    let files = event_files(config_dir, "list_usage");
    let mut u = Usage {
        files: files.len(),
        ..Usage::default()
    };
    for path in &files {
        match fs::metadata(path) {
            Ok(meta) => u.bytes += meta.len(),
            Err(error) => {
                tracing::warn!(operation = "read_usage_metadata", category = ?error.kind(), "telemetry: partial usage summary");
            }
        }
        match fs::read_to_string(path) {
            Ok(contents) => {
                u.events += contents.lines().filter(|l| !l.trim().is_empty()).count();
            }
            Err(error) => {
                tracing::warn!(operation = "read_usage_events", category = ?error.kind(), "telemetry: partial usage summary");
            }
        }
        if let Some(date) = file_date(path) {
            u.oldest = Some(u.oldest.map_or(date, |o| o.min(date)));
            u.newest = Some(u.newest.map_or(date, |n| n.max(date)));
        }
    }
    u
}

/// Render `telemetry status` output.
pub fn status_text(config_dir: &Path, enabled: bool) -> String {
    let mut out = format!(
        "Telemetry: {}\n  data directory: {}\n",
        if enabled { "enabled" } else { "disabled" },
        telemetry_dir(config_dir).display()
    );
    let u = usage(config_dir);
    if u.events == 0 {
        out.push_str("  stored:         nothing yet\n");
    } else {
        out.push_str(&format!(
            "  stored:         {} file(s), {} event(s), {}\n",
            u.files,
            u.events,
            human_bytes(u.bytes)
        ));
        if let (Some(o), Some(n)) = (u.oldest, u.newest) {
            out.push_str(&format!("  range:          {o} … {n}\n"));
        }
    }
    out
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_round_trips_as_jsonl() {
        let event = TelemetryEvent {
            at: DateTime::parse_from_rfc3339("2026-06-23T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            kind: EventKind::ToolUse {
                session_id: Some("s1".into()),
                tool: "Bash".into(),
            },
        };
        let line = serde_json::to_string(&event).unwrap();
        assert!(line.contains(r#""kind":"tool_use""#), "line = {line}");
        assert!(line.contains(r#""tool":"Bash""#));
        let back: TelemetryEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind_name(), "tool_use");
    }

    #[test]
    fn records_append_and_read_back() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record_kind(
            dir,
            EventKind::SessionStart {
                session_id: Some("s1".into()),
                agent: "claude".into(),
                plugins: vec!["tokio-plugin".into()],
            },
        );
        record_kind(
            dir,
            EventKind::UserPrompt {
                session_id: Some("s1".into()),
            },
        );
        record_kind(
            dir,
            EventKind::ToolUse {
                session_id: Some("s1".into()),
                tool: "Bash".into(),
            },
        );

        let events = read_events(dir);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind_name(), "session_start");
        assert_eq!(events[2].kind_name(), "tool_use");

        let u = usage(dir);
        assert_eq!(u.files, 1);
        assert_eq!(u.events, 3);
        assert!(u.bytes > 0);
    }

    #[test]
    fn roll_off_removes_old_files_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = telemetry_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let old = dir.join("events-2026-09-19.jsonl");
        let retained = [
            "events-2026-09-20.jsonl",
            "events-2026-10-20.jsonl",
            "events-2026-10-21.jsonl",
            ".telemetry-tmp-retention-canary",
            "unrelated.txt",
        ];
        fs::write(&old, "old\n").unwrap();
        for name in retained {
            fs::write(dir.join(name), "retained\n").unwrap();
        }

        roll_off_at(tmp.path(), RETENTION_DAYS, today).unwrap();

        assert!(!old.exists(), "old file should be removed");
        for name in retained {
            assert_eq!(fs::read(dir.join(name)).unwrap(), b"retained\n");
        }
        roll_off_at(tmp.path(), 0, today).unwrap();
        assert!(!dir.join("events-2026-09-20.jsonl").exists());
        assert!(dir.join("events-2026-10-20.jsonl").exists());
        assert!(dir.join("events-2026-10-21.jsonl").exists());
    }

    #[test]
    fn retention_cutoff_checks_duration_and_calendar_boundaries() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert_eq!(retention_cutoff(today, 0).unwrap(), today);
        assert_eq!(
            retention_cutoff(today, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
        );
        assert!(matches!(
            retention_cutoff(today, -1),
            Err(RetentionError::NegativeDays)
        ));
        assert!(matches!(
            retention_cutoff(today, i64::MAX),
            Err(RetentionError::DurationOutOfRange)
        ));
        assert!(matches!(
            retention_cutoff(NaiveDate::MIN, 1),
            Err(RetentionError::CutoffOutOfRange)
        ));
    }

    #[test]
    fn invalid_retention_preserves_all_filenames_and_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = telemetry_dir(tmp.path());
        fs::create_dir_all(&dir).unwrap();
        for (name, contents) in [
            ("events-2000-01-01.jsonl", "old\n"),
            ("events-2026-10-20.jsonl", "today\n"),
            ("events-2026-10-21.jsonl", "future\n"),
            (".telemetry-tmp-retention-canary", "temporary\n"),
            ("unrelated.txt", "unrelated\n"),
        ] {
            fs::write(dir.join(name), contents).unwrap();
        }
        let snapshot = || {
            let mut files: Vec<_> = fs::read_dir(&dir)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect();
            files.sort();
            files
        };
        let before = snapshot();
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        for days in [-1, i64::MIN, i64::MAX, 200_000_000] {
            assert!(roll_off_at(tmp.path(), days, today).is_err());
            assert_eq!(snapshot(), before, "retention_days = {days}");
            roll_off(tmp.path(), days);
            assert_eq!(snapshot(), before, "public retention_days = {days}");
        }
        assert!(roll_off_at(tmp.path(), 1, NaiveDate::MIN).is_err());
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn missing_storage_is_quiet_and_failures_have_safe_partial_diagnostics() {
        const CANARY: &str = "PRIVATE_TELEMETRY_CANARY";
        symposium_testlib::install_tracing_baseline();
        let log = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(log.reopen().unwrap())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let tmp = tempfile::tempdir().unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        assert!(read_events(tmp.path()).is_empty());
        assert_eq!(usage(tmp.path()).files, 0);
        roll_off_at(tmp.path(), RETENTION_DAYS, today).unwrap();
        assert!(fs::read_to_string(log.path()).unwrap().is_empty());

        let event = TelemetryEvent {
            at: DateTime::parse_from_rfc3339("2026-10-20T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            kind: EventKind::ToolUse {
                session_id: Some(CANARY.into()),
                tool: CANARY.into(),
            },
        };
        let blocked = tmp.path().join(format!("blocked-{CANARY}"));
        fs::write(&blocked, CANARY).unwrap();
        record(&blocked, &event);
        roll_off(&blocked, -1);
        let blocked_storage = tmp.path().join(format!("storage-{CANARY}"));
        fs::create_dir(&blocked_storage).unwrap();
        fs::write(telemetry_dir(&blocked_storage), CANARY).unwrap();
        assert!(read_events(&blocked_storage).is_empty());
        assert_eq!(usage(&blocked_storage).files, 0);
        roll_off_at(&blocked_storage, RETENTION_DAYS, today).unwrap();

        let config_dir = tmp.path().join(CANARY);
        let dir = telemetry_dir(&config_dir);
        fs::create_dir_all(&dir).unwrap();
        let valid = serde_json::to_string(&event).unwrap();
        let following = "{\"at\":\"2026-10-20T12:00:01Z\",\"kind\":\"user_prompt\"}";
        fs::write(
            file_for(&dir, event.at),
            format!("{valid}\n\n{{\"at\":\"2026-10-20T12:00:00Z\",\"kind\":\"{CANARY}\"}}\n{following}\n"),
        )
        .unwrap();
        fs::write(dir.join("events-2026-10-21.jsonl"), [0xff, 0xfe]).unwrap();
        let events = read_events(&config_dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind_name(), "tool_use");
        assert_eq!(events[1].kind_name(), "user_prompt");
        let stored = usage(&config_dir);
        assert_eq!(stored.files, 2);
        assert_eq!(stored.events, 3);

        fs::remove_file(file_for(&dir, event.at)).unwrap();
        fs::create_dir(file_for(&dir, event.at)).unwrap();
        record(&config_dir, &event);
        roll_off_at(&config_dir, 0, today.succ_opt().unwrap()).unwrap();
        drop(guard);

        let diagnostics = fs::read_to_string(log.path()).unwrap();
        for expected in [
            "create_directory",
            "list_events",
            "list_usage",
            "read_retention_directory",
            "negative_retention",
            "retention skipped",
            "parse_event",
            "category=Data",
            "line=3",
            "partial event read",
            "read_usage_events",
            "partial usage summary",
            "open_event",
            "remove_retained_file",
            "retention incomplete",
        ] {
            assert!(diagnostics.contains(expected), "missing {expected}");
        }
        assert!(!diagnostics.contains(CANARY));
        assert!(!diagnostics.contains(&valid));
    }

    #[test]
    fn status_text_reflects_enabled_and_storage() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let empty = status_text(dir, false);
        assert_eq!(
            empty,
            format!(
                "Telemetry: disabled\n  data directory: {}\n  stored:         nothing yet\n",
                telemetry_dir(dir).display()
            )
        );

        let line = "{\"at\":\"2026-10-20T12:00:00Z\",\"kind\":\"user_prompt\"}\n";
        fs::create_dir_all(telemetry_dir(dir)).unwrap();
        fs::write(telemetry_dir(dir).join("events-2026-10-20.jsonl"), line).unwrap();
        let filled = status_text(dir, true);
        assert_eq!(
            filled,
            format!(
                "Telemetry: enabled\n  data directory: {}\n  stored:         1 file(s), 1 event(s), {} B\n  range:          2026-10-20 … 2026-10-20\n",
                telemetry_dir(dir).display(),
                line.len()
            )
        );
    }
}
