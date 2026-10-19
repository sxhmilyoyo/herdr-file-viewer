//! Session Activity Log — the per-session record of file activity the viewer's own Claude Code
//! hook writes as each tool call completes, and the session view's **live source** when one
//! exists for the current session (CONTEXT.md "session activity log"). The hook side lives in
//! [`crate::hook`]; this module owns what both sides share: where the log lives, what one record
//! looks like, and how the viewer folds records into the session file set.
//!
//! One file per Claude Code session id, under the plugin's own state directory
//! (`$XDG_STATE_HOME/herdr/plugins/herdr-file-viewer/sessions`, the directory herdr hands the
//! plugin as its state dir on Linux; `%LOCALAPPDATA%` on Windows). Append-only JSON lines; the
//! viewer tails a log exactly as it tails a transcript, and a malformed or unknown record is
//! skipped, never fatal. The hook writes nothing anywhere else.

use crate::session::{Category, is_safe_session_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What one file record says happened to its path. Declaration order is the category order the
/// viewer collapses to (`read`/`mention` → mentioned, `update` → updated, `create` → created).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// A file tool read the path.
    Read,
    /// The user named the path with an `@`-mention.
    Mention,
    /// The path was written to and pre-existed (or its prior existence is unknown).
    Update,
    /// The path was born in this call.
    Create,
}

impl Activity {
    /// The session category this activity contributes.
    pub fn category(self) -> Category {
        match self {
            Activity::Read | Activity::Mention => Category::Mentioned,
            Activity::Update => Category::Updated,
            Activity::Create => Category::Created,
        }
    }
}

/// Which evidence produced a file record: a file tool's own arguments, Claude Code's Bash edit
/// record, the hook's own disk scan around a Bash call, or a prompt `@`-mention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Tool,
    BashDiff,
    Scan,
    Prompt,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// The session started (or resumed / cleared): the log exists from here on, which is what
    /// makes it the session's source even before the first tool call.
    SessionStart {
        /// Milliseconds since the Unix epoch.
        ts: u64,
        /// The session's working directory as Claude Code reported it.
        cwd: String,
    },
    /// One path the session touched.
    File {
        ts: u64,
        activity: Activity,
        /// Absolute path as the tool reported it (normalized by the reader, not here).
        path: String,
        evidence: Evidence,
        /// The tool that produced it (`Edit`, `Bash`, …); absent for prompt mentions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<String>,
        /// The subagent that made the call, when it was not the main agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
}

/// The plugin's sessions directory: `<state base>/herdr/plugins/herdr-file-viewer/sessions`.
/// unix: `$XDG_STATE_HOME`, else `$HOME/.local/state`; Windows: `%LOCALAPPDATA%`. `None` when no
/// base directory is available (then there is no log to write or read).
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(|var| std::env::var_os(var))
}

/// [`log_dir`]'s logic, factored out so it is testable from a stubbed environment. `get_env`
/// mirrors `std::env::var_os`'s signature.
pub fn log_dir_from(get_env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let base = state_base_dir(get_env)?;
    Some(
        base.join("herdr")
            .join("plugins")
            .join("herdr-file-viewer")
            .join("sessions"),
    )
}

#[cfg(not(windows))]
fn state_base_dir(get_env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    get_env("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            get_env("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local").join("state"))
        })
}

#[cfg(windows)]
fn state_base_dir(get_env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    get_env("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The log file for a session id inside `dir`, or `None` for an id that is not a safe file stem
/// (the id comes from a hook payload or a herdr reply, never from the user).
pub fn log_path(dir: &Path, session_id: &str) -> Option<PathBuf> {
    is_safe_session_id(session_id).then(|| dir.join(format!("{session_id}.jsonl")))
}

/// The session id a transcript or log file stands for: its file stem, when that is a safe id.
pub fn session_id_of(file: &Path) -> Option<&str> {
    file.file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| is_safe_session_id(s))
}

/// Fold one log line into the member map (the viewer's reader). Only `file` records count;
/// control records and anything malformed are skipped. Returns whether the map changed.
pub fn apply_line(members: &mut BTreeMap<PathBuf, Category>, line: &str) -> bool {
    let Ok(record) = serde_json::from_str::<Record>(line) else {
        return false;
    };
    match record {
        Record::File { activity, path, .. } => {
            crate::session::record(members, &path, activity.category())
        }
        Record::SessionStart { .. } => false,
    }
}

/// Serialize one record as a log line (no trailing newline). Infallible for this type in
/// practice; a serialization error yields an empty string the caller must treat as "skip".
pub fn to_line(record: &Record) -> String {
    serde_json::to_string(record).unwrap_or_default()
}

/// Whether a parsed JSON value is one of our records (used by the hook's tests and tooling).
pub fn is_record(v: &Value) -> bool {
    serde_json::from_value::<Record>(v.clone()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |var| {
            pairs
                .iter()
                .find(|(k, _)| *k == var)
                .map(|(_, v)| std::ffi::OsString::from(v))
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn log_dir_prefers_xdg_state_home_then_home_local_state() {
        assert_eq!(
            log_dir_from(env(&[
                ("XDG_STATE_HOME", "/xdg/state"),
                ("HOME", "/home/u")
            ])),
            Some(PathBuf::from(
                "/xdg/state/herdr/plugins/herdr-file-viewer/sessions"
            ))
        );
        assert_eq!(
            log_dir_from(env(&[("XDG_STATE_HOME", ""), ("HOME", "/home/u")])),
            Some(PathBuf::from(
                "/home/u/.local/state/herdr/plugins/herdr-file-viewer/sessions"
            ))
        );
        assert_eq!(log_dir_from(env(&[])), None);
    }

    #[test]
    fn log_path_requires_a_safe_session_id() {
        let dir = Path::new("/s");
        assert_eq!(
            log_path(dir, "3a744a71-5cce-481b-8107-db5bf2f355d9"),
            Some(PathBuf::from(
                "/s/3a744a71-5cce-481b-8107-db5bf2f355d9.jsonl"
            ))
        );
        assert_eq!(log_path(dir, "../x"), None);
        assert_eq!(log_path(dir, ""), None);
        assert_eq!(
            session_id_of(Path::new("/p/3a744a71-5cce-481b-8107-db5bf2f355d9.jsonl")),
            Some("3a744a71-5cce-481b-8107-db5bf2f355d9")
        );
        assert_eq!(session_id_of(Path::new("/p/..")), None);
    }

    #[test]
    fn records_round_trip_as_single_json_lines() {
        let rec = Record::File {
            ts: 1_700_000_000_000,
            activity: Activity::Create,
            path: "/r/new.rs".into(),
            evidence: Evidence::Tool,
            tool: Some("Write".into()),
            agent_id: None,
        };
        let line = to_line(&rec);
        assert!(!line.contains('\n'));
        assert!(line.contains(r#""kind":"file""#) && line.contains(r#""activity":"create""#));
        assert!(!line.contains("agent_id"), "absent optionals are omitted");
        assert_eq!(serde_json::from_str::<Record>(&line).unwrap(), rec);
        let start = to_line(&Record::SessionStart {
            ts: 1,
            cwd: "/r".into(),
        });
        assert!(start.contains(r#""kind":"session_start""#));
    }

    #[test]
    fn reader_folds_file_records_and_skips_the_rest() {
        let mut members = BTreeMap::new();
        let lines = [
            r#"{"kind":"session_start","ts":1,"cwd":"/r"}"#,
            r#"{"kind":"file","ts":2,"activity":"read","path":"/r/a.rs","evidence":"tool","tool":"Read"}"#,
            r#"{"kind":"file","ts":3,"activity":"update","path":"/r/a.rs","evidence":"scan","tool":"Bash","agent_id":"agent-1"}"#,
            r#"{"kind":"file","ts":4,"activity":"create","path":"/r/b.rs","evidence":"bash_diff","tool":"Bash"}"#,
            r#"{"kind":"file","ts":5,"activity":"mention","path":"/etc/hosts","evidence":"prompt"}"#,
            r#"{"kind":"file","ts":6,"activity":"update","path":"relative.rs","evidence":"tool"}"#,
            r#"{"kind":"unknown","ts":7}"#,
            "not json",
        ];
        let changes: Vec<bool> = lines.iter().map(|l| apply_line(&mut members, l)).collect();
        assert_eq!(
            changes,
            [false, true, true, true, true, false, false, false]
        );
        assert_eq!(members.get(Path::new("/r/a.rs")), Some(&Category::Updated));
        assert_eq!(members.get(Path::new("/r/b.rs")), Some(&Category::Created));
        assert_eq!(
            members.get(Path::new("/etc/hosts")),
            Some(&Category::Mentioned)
        );
        assert_eq!(members.len(), 3);
    }
}
