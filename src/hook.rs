//! Claude Code hook — the writer side of the **session activity log** (`herdr-file-viewer hook …`).
//!
//! `hook run` is what Claude Code invokes, with the documented hook payload on stdin, on
//! `SessionStart`, `PreToolUse` (Bash), `PostToolUse` (Edit, Write, MultiEdit, NotebookEdit, Read,
//! Bash) and `UserPromptSubmit`. It appends records to the session's log and exits 0 with no output
//! in every case: a hook on the agent's critical path must never block or fail a tool call, so
//! every error degrades to "record nothing". `hook install | uninstall | status` edit or read the
//! user-scope Claude Code settings file — the one explicit, reversible opt-in the constitution asks
//! for (the viewer itself stays read-only; the hook writes only under the plugin's own state dir).
//!
//! Membership follows CONTEXT.md "touched": file tools name their path; a Bash call contributes
//! Claude Code's own edit record (`tool_response.bashEditDiff`) plus the files observed changing on
//! disk between the call's start and end (the stamp written at `PreToolUse`); a prompt contributes
//! its `@`-mentions. Shell *reads* are never inferred.

use crate::activity::{self, Activity, Evidence, Record};
use crate::git::git_command;
use ignore::WalkBuilder;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The `hook` subcommands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookCommand {
    /// Claude Code's entry point: read one payload from stdin, append records, exit 0.
    Run,
    /// Register the hook in the user-scope Claude Code settings (idempotent).
    Install,
    /// Remove every entry this plugin registered (leaves everything else untouched).
    Uninstall,
    /// Report whether the hook is registered and whether the registered binary still exists.
    Status,
    /// Print usage (an unknown or missing subcommand).
    Help,
}

/// The events and matchers the hook registers for.
pub const EVENTS: [(&str, &str); 4] = [
    ("SessionStart", ""),
    ("PreToolUse", "Bash"),
    ("PostToolUse", "Edit|Write|MultiEdit|NotebookEdit|Read|Bash"),
    ("UserPromptSubmit", ""),
];

/// Seconds Claude Code waits for one hook invocation before giving up on it.
const HOOK_TIMEOUT_SECS: u64 = 10;

/// The most files one Bash call's disk scan records (Claude Code caps its own edit record at the
/// same number).
const SCAN_CAP: usize = 200;

/// Depth of the scan when the Bash call's directory is not inside a git repository.
const SCAN_DEPTH_OUTSIDE_REPO: usize = 4;

/// Directory names the scan never enters: build output and dependency trees that churn on every
/// command and are never what a session "touched".
const PRUNED_DIRS: [&str; 8] = [
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    ".venv",
    "__pycache__",
    "env",
];

/// A stamp older than this is garbage from a Bash call whose `PostToolUse` never ran.
const STALE_STAMP: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

/// Run a `hook` subcommand; returns the process exit code. `Run` is always 0 and silent.
pub fn main(cmd: HookCommand) -> i32 {
    match cmd {
        HookCommand::Run => {
            let _ = run(std::io::stdin().lock());
            0
        }
        HookCommand::Install => report(install()),
        HookCommand::Uninstall => report(uninstall()),
        HookCommand::Status => report(status_report()),
        HookCommand::Help => {
            eprintln!(
                "usage: herdr-file-viewer hook <run|install|uninstall|status>\n  \
                 install    register the session hook in Claude Code's user settings\n  \
                 uninstall  remove it again\n  \
                 status     show whether it is registered\n  \
                 run        the hook itself (Claude Code calls this)"
            );
            2
        }
    }
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}

// ---------------------------------------------------------------------------------------------
// `hook run`: payload → records → log
// ---------------------------------------------------------------------------------------------

/// Process one payload. Every `None` is a silent no-op by design.
fn run(mut stdin: impl Read) -> Option<()> {
    let mut raw = String::new();
    stdin.read_to_string(&mut raw).ok()?;
    let payload: Value = serde_json::from_str(&raw).ok()?;
    let session_id = payload.get("session_id")?.as_str()?;
    let dir = activity::log_dir()?;
    let log = activity::log_path(&dir, session_id)?;
    let now = SystemTime::now();
    let event = payload.get("hook_event_name")?.as_str()?;
    let tool = payload.get("tool_name").and_then(Value::as_str);
    let records = match (event, tool) {
        ("PreToolUse", Some("Bash")) => {
            fs::create_dir_all(&dir).ok()?;
            write_stamp(&dir, session_id, tool_use_id(&payload), now);
            return Some(());
        }
        ("PostToolUse", Some("Bash")) => {
            let cwd = payload_cwd(&payload)?;
            let scanned = take_stamp(&dir, session_id, tool_use_id(&payload))
                .map(|since| scan_changed_since(cwd, since))
                .unwrap_or_default();
            records_for(&payload, millis(now), &scanned, &|p| p.is_file())
        }
        ("SessionStart", _) => {
            prune_stale_stamps(&dir, session_id, now);
            records_for(&payload, millis(now), &[], &|p| p.is_file())
        }
        _ => records_for(&payload, millis(now), &[], &|p| p.is_file()),
    };
    if records.is_empty() {
        return Some(());
    }
    fs::create_dir_all(&dir).ok()?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .ok()?;
    let mut buf = String::new();
    for record in &records {
        let line = activity::to_line(record);
        if !line.is_empty() {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    file.write_all(buf.as_bytes()).ok()
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn payload_cwd(payload: &Value) -> Option<&Path> {
    payload
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(Path::new)
}

fn tool_use_id(payload: &Value) -> Option<&str> {
    payload.get("tool_use_id").and_then(Value::as_str)
}

/// The records one payload contributes (pure). `scanned` is the Bash call's disk scan, already
/// classified `(path, created)`; `exists` answers whether an `@`-mentioned path is a file.
pub fn records_for(
    payload: &Value,
    now_ms: u64,
    scanned: &[(PathBuf, bool)],
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Record> {
    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let cwd = payload_cwd(payload);
    let agent_id = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut out = Vec::new();
    let mut file = |activity: Activity, path: &Path, evidence: Evidence, tool: Option<&str>| {
        let Some(abs) = absolute(path, cwd) else {
            return;
        };
        out.push(Record::File {
            ts: now_ms,
            activity,
            path: abs.to_string_lossy().into_owned(),
            evidence,
            tool: tool.map(str::to_owned),
            agent_id: agent_id.clone(),
        });
    };
    match event {
        "SessionStart" => {
            if let Some(cwd) = cwd {
                out.push(Record::SessionStart {
                    ts: now_ms,
                    cwd: cwd.to_string_lossy().into_owned(),
                });
            }
        }
        "PostToolUse" => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let input = payload.get("tool_input");
            let response = payload.get("tool_response");
            let input_path = |key: &str| {
                input
                    .and_then(|i| i.get(key))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(Path::new)
            };
            match tool {
                "Edit" | "MultiEdit" => {
                    if let Some(p) = input_path("file_path") {
                        file(Activity::Update, p, Evidence::Tool, Some(tool));
                    }
                }
                "Write" => {
                    if let Some(p) = input_path("file_path") {
                        // Write's response says whether it created the file; absent, assume an
                        // overwrite (updated), the weaker claim.
                        let created = response.and_then(|r| r.get("type")).and_then(Value::as_str)
                            == Some("create");
                        let activity = if created {
                            Activity::Create
                        } else {
                            Activity::Update
                        };
                        file(activity, p, Evidence::Tool, Some(tool));
                    }
                }
                "NotebookEdit" => {
                    if let Some(p) = input_path("notebook_path") {
                        file(Activity::Update, p, Evidence::Tool, Some(tool));
                    }
                }
                "Read" => {
                    if let Some(p) = input_path("file_path") {
                        file(Activity::Read, p, Evidence::Tool, Some(tool));
                    }
                }
                "Bash" => {
                    if let Some(diff) = response
                        .and_then(|r| r.get("bashEditDiff"))
                        .filter(|d| d.is_object())
                    {
                        for p in diff
                            .get("changedFiles")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                        {
                            file(
                                Activity::Update,
                                Path::new(p),
                                Evidence::BashDiff,
                                Some(tool),
                            );
                        }
                        for entry in diff
                            .get("files")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            let Some(p) = entry.get("filePath").and_then(Value::as_str) else {
                                continue;
                            };
                            let created =
                                entry.get("created").and_then(Value::as_bool) == Some(true);
                            let activity = if created {
                                Activity::Create
                            } else {
                                Activity::Update
                            };
                            file(activity, Path::new(p), Evidence::BashDiff, Some(tool));
                        }
                    }
                    for (p, created) in scanned {
                        let activity = if *created {
                            Activity::Create
                        } else {
                            Activity::Update
                        };
                        file(activity, p, Evidence::Scan, Some(tool));
                    }
                }
                _ => {}
            }
        }
        "UserPromptSubmit" => {
            let prompt = payload.get("prompt").and_then(Value::as_str).unwrap_or("");
            for raw in mentions(prompt) {
                let candidate = Path::new(raw);
                if let Some(abs) = absolute(candidate, cwd)
                    && exists(&abs)
                {
                    file(Activity::Mention, &abs, Evidence::Prompt, None);
                }
            }
        }
        _ => {}
    }
    out
}

/// Make a tool-reported path absolute against the payload's cwd (tools report absolute paths; a
/// relative one is resolved rather than dropped, since the hook knows the cwd the viewer does not).
fn absolute(path: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    cwd.map(|c| c.join(path))
}

/// The `@`-mention candidates in a prompt: whitespace-delimited tokens whose `@` comes first
/// (after an opening bracket or quote, so `(@path)` counts), with trailing punctuation stripped.
/// An email address has its `@` mid-token and is never a candidate; a handle that slips through
/// is dropped by the caller's existence check, since it names no file.
pub fn mentions(prompt: &str) -> Vec<&str> {
    prompt
        .split_whitespace()
        .filter_map(|tok| {
            tok.trim_start_matches(['(', '[', '{', '\'', '"'])
                .strip_prefix('@')
        })
        .map(|tok| tok.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"']))
        .filter(|tok| !tok.is_empty() && !tok.starts_with('@'))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Bash call stamps + disk scan
// ---------------------------------------------------------------------------------------------

/// A tool-use id is a safe file-name component iff it is `[A-Za-z0-9_-]` and non-empty.
fn safe_component(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn stamp_path(dir: &Path, session_id: &str, tool_use_id: Option<&str>) -> PathBuf {
    let call = tool_use_id
        .filter(|id| safe_component(id))
        .unwrap_or("bash");
    dir.join(format!("{session_id}.{call}.stamp"))
}

/// Record when a Bash call started so its `PostToolUse` can bound the disk scan.
pub fn write_stamp(dir: &Path, session_id: &str, tool_use_id: Option<&str>, at: SystemTime) {
    let _ = fs::write(
        stamp_path(dir, session_id, tool_use_id),
        millis(at).to_string(),
    );
}

/// Read and remove the matching stamp; `None` when the `PreToolUse` side never ran.
pub fn take_stamp(dir: &Path, session_id: &str, tool_use_id: Option<&str>) -> Option<SystemTime> {
    let path = stamp_path(dir, session_id, tool_use_id);
    let raw = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let ms: u64 = raw.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(ms))
}

/// Drop this session's stamps older than [`STALE_STAMP`] (a Bash call whose end was never seen).
fn prune_stale_stamps(dir: &Path, session_id: &str, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{session_id}.");
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with(&prefix) && name.ends_with(".stamp")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > STALE_STAMP);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The files under the Bash call's root whose mtime is at or after `since`, each flagged
/// `created` when git reports it untracked. Root = the git top level of `cwd` when inside a
/// repository (full depth), else `cwd` itself to [`SCAN_DEPTH_OUTSIDE_REPO`]. `.gitignore` rules
/// apply either way and [`PRUNED_DIRS`] are never entered; at most [`SCAN_CAP`] paths.
pub fn scan_changed_since(cwd: &Path, since: SystemTime) -> Vec<(PathBuf, bool)> {
    let (root, in_repo) = match repo_root(cwd) {
        Some(root) => (root, true),
        None => (cwd.to_path_buf(), false),
    };
    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(false)
        .parents(true)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| PRUNED_DIRS.contains(&n)))
        });
    if !in_repo {
        builder.max_depth(Some(SCAN_DEPTH_OUTSIDE_REPO));
    }
    let mut changed: Vec<PathBuf> = Vec::new();
    for entry in builder.build().filter_map(Result::ok) {
        if changed.len() >= SCAN_CAP {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let fresh = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|m| m >= since);
        if fresh {
            changed.push(entry.into_path());
        }
    }
    changed.sort();
    let untracked = if in_repo && !changed.is_empty() {
        untracked_among(&root, &changed)
    } else {
        BTreeSet::new()
    };
    changed
        .into_iter()
        .map(|p| {
            let created = untracked.contains(&p);
            (p, created)
        })
        .collect()
}

/// The git top level containing `dir`, if any (hardened invocation, see `git::git_command`).
fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git_command(dir, &["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Which of `paths` git reports as untracked (`??`) — the created/updated discriminator for files
/// the scan found. One `git status --porcelain -z` over the paths; a failing git means "none".
fn untracked_among(root: &Path, paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut args: Vec<&str> = vec!["status", "--porcelain", "-z", "--untracked-files=all", "--"];
    let rels: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    args.extend(rels.iter().map(String::as_str));
    let Ok(out) = git_command(root, &args).output() else {
        return BTreeSet::new();
    };
    if !out.status.success() {
        return BTreeSet::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split('\0')
        .filter_map(|entry| entry.strip_prefix("?? "))
        .map(|rel| root.join(rel))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Settings: install / uninstall / status
// ---------------------------------------------------------------------------------------------

/// The user-scope Claude Code settings file: `$CLAUDE_CONFIG_DIR/settings.json`, else
/// `<home>/.claude/settings.json` (`$HOME`; `%USERPROFILE%` on Windows).
pub fn settings_path_from(get_env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let dir = get_env("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            let home = "USERPROFILE";
            #[cfg(not(windows))]
            let home = "HOME";
            get_env(home)
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".claude"))
        })?;
    Some(dir.join("settings.json"))
}

fn settings_path() -> Option<PathBuf> {
    settings_path_from(|var| std::env::var_os(var))
}

/// The command line Claude Code should run: the given binary, double-quoted so a path with
/// spaces survives the shell, followed by `hook run`.
pub fn hook_command(exe: &Path) -> String {
    let raw = exe.to_string_lossy();
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    for c in raw.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    format!("{quoted} hook run")
}

/// Whether a registered command is one of ours: it runs a `herdr-file-viewer` binary's
/// `hook run`. The marker is the command shape itself, so no extra bookkeeping key is needed.
pub fn is_ours(command: &str) -> bool {
    command.trim_end().ends_with(" hook run") && command.contains("herdr-file-viewer")
}

/// Remove every entry of ours from `settings["hooks"]`; returns whether anything was removed.
/// Entries of other tools are untouched; an entry left with no hooks is dropped; an event left
/// with no entries is dropped; an empty `hooks` object is dropped.
pub fn uninstall_from(settings: &mut Value) -> bool {
    let mut removed = false;
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for entry in entries.iter_mut() {
            if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_ours)
                });
                removed |= inner.len() != before;
            }
        }
        entries.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|inner| !inner.is_empty())
        });
        if entries.is_empty() {
            hooks.remove(&event);
        }
    }
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
    removed
}

/// Register `command` for every event in [`EVENTS`], after removing any earlier registration of
/// ours (so a re-install after the binary moved is one step). Returns whether the settings
/// changed. A non-object `settings` is left alone and reported unchanged.
pub fn install_into(settings: &mut Value, command: &str) -> bool {
    if !settings.is_object() {
        return false;
    }
    let before = settings.clone();
    uninstall_from(settings);
    let root = settings.as_object_mut().expect("checked is_object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()));
    if !hooks.is_object() {
        *hooks = Value::Object(Default::default());
    }
    let hooks = hooks.as_object_mut().expect("made an object");
    for (event, matcher) in EVENTS {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entries.is_array() {
            *entries = Value::Array(Vec::new());
        }
        entries.as_array_mut().expect("made an array").push(json!({
            "matcher": matcher,
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": HOOK_TIMEOUT_SECS,
            }],
        }));
    }
    *settings != before
}

/// The command this plugin has registered, if any (the first match wins; every event carries
/// the same command after an `install`).
pub fn installed_command(settings: &Value) -> Option<String> {
    let hooks = settings.get("hooks")?.as_object()?;
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .find(|c| is_ours(c))
        .map(str::to_owned)
}

/// The binary path a registered command names (the quoted first word), for the staleness check.
pub fn registered_binary(command: &str) -> Option<PathBuf> {
    let rest = command.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(PathBuf::from(out)),
            c => out.push(c),
        }
    }
    None
}

/// Whether the hook is registered in the user's settings: `Some(true|false)`, or `None` when the
/// settings file cannot be read or parsed (then the viewer shows nothing rather than a guess).
pub fn status() -> Option<bool> {
    let path = settings_path()?;
    let settings = read_settings(&path).ok()?;
    Some(installed_command(&settings).is_some())
}

fn read_settings(path: &Path) -> Result<Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            format!(
                "{}: not valid JSON ({e}); refusing to touch it",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write the settings atomically (temp file in the same directory, then rename) so a crash
/// mid-write can never leave Claude Code with a truncated settings file.
fn write_settings(path: &Path, settings: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    let dir = path.parent().ok_or("settings path has no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".settings.json.herdr-file-viewer-{}",
        std::process::id()
    ));
    fs::write(&tmp, format!("{text}\n")).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn install() -> Result<String, String> {
    let path = settings_path().ok_or("cannot locate Claude Code's settings (no home directory)")?;
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let command = hook_command(&exe);
    let mut settings = read_settings(&path)?;
    if installed_command(&settings).as_deref() == Some(command.as_str()) {
        return Ok(format!(
            "Session hook already installed in {}:\n  {command}",
            path.display()
        ));
    }
    install_into(&mut settings, &command);
    write_settings(&path, &settings)?;
    Ok(format!(
        "Session hook installed in {} for {}:\n  {command}\nRunning Claude Code sessions pick it up after a restart (or `/hooks`).",
        path.display(),
        EVENTS
            .iter()
            .map(|(e, _)| *e)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn uninstall() -> Result<String, String> {
    let path = settings_path().ok_or("cannot locate Claude Code's settings (no home directory)")?;
    let mut settings = read_settings(&path)?;
    if !uninstall_from(&mut settings) {
        return Ok(format!("No session hook found in {}", path.display()));
    }
    write_settings(&path, &settings)?;
    Ok(format!("Session hook removed from {}", path.display()))
}

fn status_report() -> Result<String, String> {
    let path = settings_path().ok_or("cannot locate Claude Code's settings (no home directory)")?;
    let settings = read_settings(&path)?;
    match installed_command(&settings) {
        None => Ok(format!(
            "not installed ({}); run: herdr-file-viewer hook install",
            path.display()
        )),
        Some(command) => {
            let stale = registered_binary(&command).is_some_and(|bin| !bin.is_file());
            let note = if stale {
                "\n  the registered binary no longer exists: run `herdr-file-viewer hook install` again"
            } else {
                ""
            };
            Ok(format!(
                "installed ({}):\n  {command}{note}",
                path.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(event: &str, extra: Value) -> Value {
        let mut v = json!({
            "session_id": "3a744a71-5cce-481b-8107-db5bf2f355d9",
            "transcript_path": "/home/u/.claude/projects/-r/3a744a71-5cce-481b-8107-db5bf2f355d9.jsonl",
            "cwd": "/r",
            "hook_event_name": event,
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    fn paths(records: &[Record]) -> Vec<(String, Activity, Evidence)> {
        records
            .iter()
            .filter_map(|r| match r {
                Record::File {
                    path,
                    activity,
                    evidence,
                    ..
                } => Some((path.clone(), *activity, *evidence)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn session_start_opens_the_log_with_the_cwd() {
        let recs = records_for(
            &payload("SessionStart", json!({"source": "startup"})),
            7,
            &[],
            &|_| false,
        );
        assert_eq!(
            recs,
            [Record::SessionStart {
                ts: 7,
                cwd: "/r".into()
            }]
        );
    }

    #[test]
    fn file_tools_report_their_path_with_the_right_activity() {
        let edit = payload(
            "PostToolUse",
            json!({"tool_name": "Edit", "tool_input": {"file_path": "/r/a.rs", "old_string": "x", "new_string": "y"}, "tool_response": {"filePath": "/r/a.rs"}}),
        );
        let multi = payload(
            "PostToolUse",
            json!({"tool_name": "MultiEdit", "tool_input": {"file_path": "/r/m.rs", "edits": []}}),
        );
        let write_new = payload(
            "PostToolUse",
            json!({"tool_name": "Write", "tool_input": {"file_path": "/r/new.rs", "content": ""}, "tool_response": {"type": "create", "filePath": "/r/new.rs"}}),
        );
        let write_over = payload(
            "PostToolUse",
            json!({"tool_name": "Write", "tool_input": {"file_path": "/r/old.rs", "content": ""}, "tool_response": {"type": "update", "filePath": "/r/old.rs"}}),
        );
        let notebook = payload(
            "PostToolUse",
            json!({"tool_name": "NotebookEdit", "tool_input": {"notebook_path": "/r/n.ipynb"}}),
        );
        let read = payload(
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {"file_path": "relative/b.rs"}, "agent_id": "agent-1"}),
        );
        let got: Vec<_> = [edit, multi, write_new, write_over, notebook, read]
            .iter()
            .flat_map(|p| records_for(p, 1, &[], &|_| true))
            .collect();
        assert_eq!(
            paths(&got),
            [
                ("/r/a.rs".to_string(), Activity::Update, Evidence::Tool),
                ("/r/m.rs".to_string(), Activity::Update, Evidence::Tool),
                ("/r/new.rs".to_string(), Activity::Create, Evidence::Tool),
                ("/r/old.rs".to_string(), Activity::Update, Evidence::Tool),
                ("/r/n.ipynb".to_string(), Activity::Update, Evidence::Tool),
                // A relative tool path is resolved against the payload's cwd, not dropped.
                (
                    "/r/relative/b.rs".to_string(),
                    Activity::Read,
                    Evidence::Tool
                ),
            ]
        );
        // The subagent id rides along on its records only.
        let last = got.last().unwrap();
        assert!(
            matches!(last, Record::File { agent_id: Some(a), tool: Some(t), .. } if a == "agent-1" && t == "Read")
        );
        assert!(matches!(&got[0], Record::File { agent_id: None, .. }));
    }

    #[test]
    fn bash_records_claude_codes_edit_record_and_the_scan() {
        let bash = payload(
            "PostToolUse",
            json!({
                "tool_name": "Bash",
                "tool_input": {"command": "sed -i s/a/b/ /r/a.rs"},
                "tool_response": {
                    "stdout": "", "stderr": "", "interrupted": false, "isImage": false,
                    "bashEditDiff": {
                        "changedFiles": ["/r/a.rs", "/r/b.rs"],
                        "files": [{"filePath": "/r/b.rs", "created": true, "hunks": []}],
                        "moreFiles": 0
                    }
                }
            }),
        );
        let scanned = vec![
            (PathBuf::from("/r/gen/out.rs"), true),
            (PathBuf::from("/r/a.rs"), false),
        ];
        let got = records_for(&bash, 1, &scanned, &|_| true);
        assert_eq!(
            paths(&got),
            [
                ("/r/a.rs".to_string(), Activity::Update, Evidence::BashDiff),
                ("/r/b.rs".to_string(), Activity::Update, Evidence::BashDiff),
                ("/r/b.rs".to_string(), Activity::Create, Evidence::BashDiff),
                (
                    "/r/gen/out.rs".to_string(),
                    Activity::Create,
                    Evidence::Scan
                ),
                ("/r/a.rs".to_string(), Activity::Update, Evidence::Scan),
            ],
            "duplicates are fine: the reader keeps the strongest category per path"
        );
        // A read-only Bash call with no record and nothing scanned contributes nothing.
        let quiet = payload(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": {"stdout": "x", "stderr": ""}}),
        );
        assert!(records_for(&quiet, 1, &[], &|_| true).is_empty());
    }

    #[test]
    fn prompt_mentions_count_only_when_they_name_an_existing_file() {
        let p = payload(
            "UserPromptSubmit",
            json!({"prompt": "look at @src/a.rs, @/abs/b.md and @missing.rs; ping @alice and mail x@y.z"}),
        );
        let exists = |path: &Path| matches!(path.to_str(), Some("/r/src/a.rs" | "/abs/b.md"));
        let got = records_for(&p, 1, &[], &exists);
        assert_eq!(
            paths(&got),
            [
                (
                    "/r/src/a.rs".to_string(),
                    Activity::Mention,
                    Evidence::Prompt
                ),
                ("/abs/b.md".to_string(), Activity::Mention, Evidence::Prompt),
            ]
        );
        assert_eq!(mentions("@a.rs. (@b) @@x @"), ["a.rs", "b"]);
    }

    #[test]
    fn unrelated_events_and_tools_contribute_nothing() {
        for p in [
            payload(
                "PreToolUse",
                json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            ),
            payload(
                "PostToolUse",
                json!({"tool_name": "Grep", "tool_input": {"pattern": "x", "path": "/r"}}),
            ),
            payload(
                "PostToolUse",
                json!({"tool_name": "Agent", "tool_input": {"prompt": "x"}}),
            ),
            payload("Stop", json!({})),
            json!({"not": "a payload"}),
        ] {
            assert!(records_for(&p, 1, &[], &|_| true).is_empty(), "{p}");
        }
    }

    #[test]
    fn hook_command_quotes_the_binary_and_is_recognized_as_ours() {
        let cmd = hook_command(Path::new("/opt/my tools/herdr-file-viewer"));
        assert_eq!(cmd, "\"/opt/my tools/herdr-file-viewer\" hook run");
        assert!(is_ours(&cmd));
        assert_eq!(
            registered_binary(&cmd),
            Some(PathBuf::from("/opt/my tools/herdr-file-viewer"))
        );
        let tricky = hook_command(Path::new("/x/herdr-file-viewer$\"`"));
        assert_eq!(tricky, "\"/x/herdr-file-viewer\\$\\\"\\`\" hook run");
        assert_eq!(
            registered_binary(&tricky),
            Some(PathBuf::from("/x/herdr-file-viewer$\"`"))
        );
        assert!(!is_ours(
            "bash '/home/u/.claude/hooks/herdr-agent-state.sh' session"
        ));
        assert!(!is_ours("prettier --write"));
    }

    #[test]
    fn install_is_idempotent_and_uninstall_removes_only_ours() {
        let theirs =
            json!({"matcher": "", "hooks": [{"type": "command", "command": "notify-send hi"}]});
        let herdr = json!({"matcher": "^(startup|resume)$", "hooks": [{"type": "command", "command": "bash '/h/herdr-agent-state.sh' session", "timeout": 10}]});
        let mut settings = json!({
            "model": "opus",
            "hooks": {"Notification": [theirs.clone()], "SessionStart": [herdr.clone()]}
        });
        let cmd = hook_command(Path::new("/p/herdr-file-viewer"));

        assert!(install_into(&mut settings, &cmd));
        let once = settings.clone();
        assert!(
            !install_into(&mut settings, &cmd),
            "second install changes nothing"
        );
        assert_eq!(settings, once);
        assert_eq!(installed_command(&settings).as_deref(), Some(cmd.as_str()));
        // Every event carries exactly one entry of ours, alongside the others.
        for (event, matcher) in EVENTS {
            let entries = settings["hooks"][event].as_array().unwrap();
            let ours: Vec<_> = entries
                .iter()
                .filter(|e| e["hooks"][0]["command"] == json!(cmd))
                .collect();
            assert_eq!(ours.len(), 1, "{event}");
            assert_eq!(ours[0]["matcher"], json!(matcher));
            assert_eq!(ours[0]["hooks"][0]["timeout"], json!(HOOK_TIMEOUT_SECS));
        }
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            2
        );

        // A moved binary: re-install replaces the old registration instead of stacking one.
        let moved = hook_command(Path::new("/q/herdr-file-viewer"));
        assert!(install_into(&mut settings, &moved));
        assert_eq!(
            installed_command(&settings).as_deref(),
            Some(moved.as_str())
        );
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);

        assert!(uninstall_from(&mut settings));
        assert_eq!(installed_command(&settings), None);
        assert_eq!(settings["hooks"]["Notification"], json!([theirs]));
        assert_eq!(settings["hooks"]["SessionStart"], json!([herdr]));
        assert!(
            settings["hooks"].get("PreToolUse").is_none(),
            "emptied events are dropped"
        );
        assert_eq!(settings["model"], json!("opus"));
        assert!(!uninstall_from(&mut settings), "nothing left to remove");

        // Settings without a hooks object gain one; a lone hooks object is removed with its last entry.
        let mut bare = json!({"theme": "dark"});
        assert!(install_into(&mut bare, &cmd));
        assert!(uninstall_from(&mut bare));
        assert_eq!(bare, json!({"theme": "dark"}));
        let mut not_object = json!([1, 2]);
        assert!(!install_into(&mut not_object, &cmd));
    }

    #[test]
    fn settings_path_honors_claude_config_dir_then_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == var)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        assert_eq!(
            settings_path_from(env(&[("CLAUDE_CONFIG_DIR", "/cfg"), ("HOME", "/home/u")])),
            Some(PathBuf::from("/cfg/settings.json"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            settings_path_from(env(&[("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.claude/settings.json"))
        );
        assert_eq!(settings_path_from(env(&[])), None);
    }

    #[test]
    fn stamp_names_are_safe_components() {
        let dir = Path::new("/s");
        assert_eq!(
            stamp_path(dir, "sid", Some("toolu_01AbC")),
            PathBuf::from("/s/sid.toolu_01AbC.stamp")
        );
        assert_eq!(
            stamp_path(dir, "sid", Some("../x")),
            PathBuf::from("/s/sid.bash.stamp"),
            "a hostile id falls back to the session-level stamp"
        );
        assert_eq!(
            stamp_path(dir, "sid", None),
            PathBuf::from("/s/sid.bash.stamp")
        );
    }
}
