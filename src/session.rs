//! Session Transcript Reader — locate Claude Code's per-project session transcripts and fold
//! them into the **session file set** the session view presents (CONTEXT.md), split into
//! in-root and outside-root members, each carrying its strongest **session category**.
//!
//! The transcript format is an undocumented Claude Code internal (ADR-0011), so everything here
//! is deliberately fail-soft: a malformed line, an unknown entry shape, or any I/O error is
//! skipped or degrades to an empty result — never a panic, never a partial crash. Read-only:
//! this module only ever reads transcript files; it never writes anything (AC-N1 spirit).
//!
//! Membership is deterministic only (CONTEXT.md "session file set"): agent file-tool *results*
//! (a denied or failed call produces a string result, so it never counts), user `@`-mention
//! attachments, and subagent activity — Task-tool sidechains live in **separate transcript
//! files** (`<dir>/<session-id>/subagents/*.jsonl`), which the [`Follower`] discovers and
//! follows alongside the main file. Shell-command side effects are never inferred.

use crate::open_target::lexically_normalize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The one label a session-file-set member displays. Declaration order is strength order
/// (`Mentioned < Updated < Created`, CONTEXT.md "session category"): a file read and later
/// edited is *updated*; a file born in this session stays *created* even if edited afterward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    /// Read by a file tool, or named by a user `@`-mention, and never written to.
    Mentioned,
    /// Written to, but the file pre-existed the session.
    Updated,
    /// Born in this session (a Write that created the file).
    Created,
}

impl Category {
    /// The one-glyph tree marker (`+` created, `~` updated, `·` mentioned).
    pub fn glyph(self) -> char {
        match self {
            Category::Created => '+',
            Category::Updated => '~',
            Category::Mentioned => '·',
        }
    }
}

/// The derived set of files the current session touched, partitioned around the tree root:
/// members under it (keyed **root-relative**, like the changed-set, so the synthesized tree and
/// git decorations compose) and members outside it (keyed absolute, for the outside-root
/// section).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSet {
    pub in_root: BTreeMap<PathBuf, Category>,
    pub outside: BTreeMap<PathBuf, Category>,
}

impl SessionSet {
    pub fn is_empty(&self) -> bool {
        self.in_root.is_empty() && self.outside.is_empty()
    }

    /// Total member count, both sides.
    pub fn len(&self) -> usize {
        self.in_root.len() + self.outside.len()
    }
}

/// Claude Code's project-directory slug for a working directory: every non-alphanumeric
/// character becomes `-` (verified against real `~/.claude/projects` entries — `/` and `.`
/// both map to `-`, so `/a/.claude` is `-a--claude`). Mapped per **UTF-16 code unit** to
/// reproduce the JS `replace(/[^a-zA-Z0-9]/g, '-')` exactly: an astral character (an emoji in
/// a directory name) is TWO units and so two dashes — a per-`char` map would emit one and
/// never match the real store. Lossy but derived forward only (cwd → slug), never inverted.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy()
        .encode_utf16()
        .map(|u| match u {
            0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A => u as u8 as char,
            _ => '-',
        })
        .collect()
}

/// Claude Code's project store: `<home>/.claude/projects`, one slug directory per working dir.
pub fn projects_root(home: &Path) -> PathBuf {
    home.join(".claude").join("projects")
}

/// Where Claude Code keeps `root`'s session transcripts: `<home>/.claude/projects/<slug>`.
pub fn projects_dir(home: &Path, root: &Path) -> PathBuf {
    projects_root(home).join(project_slug(root))
}

/// A session id is safe to use as a transcript file stem iff it is a non-empty token of
/// `[A-Za-z0-9-]` (Claude Code session ids are UUIDs). Anything else — a path separator, a dot
/// segment, a flag-shaped prefix — is refused rather than joined into a path.
pub fn is_safe_session_id(id: &str) -> bool {
    id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The transcript for a session id: `<slug dir of root>/<id>.jsonl` when it exists (the common
/// case), else the first `<projects>/*/<id>.jsonl` found — a session started in a subdirectory
/// of the root, or in another worktree, lives under a different slug than the viewer derives.
/// One file per session id across the store (verified), so the first hit is the only hit. `None`
/// for an unsafe id or when no project directory holds it (a brand-new session that has not
/// written its transcript yet degrades to the caller's fallback).
pub fn transcript_for_session_id(home: &Path, root: &Path, id: &str) -> Option<PathBuf> {
    if !is_safe_session_id(id) {
        return None;
    }
    let file = format!("{id}.jsonl");
    let direct = projects_dir(home, root).join(&file);
    if direct.is_file() {
        return Some(direct);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(projects_root(home))
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort(); // deterministic when the store is scanned
    dirs.into_iter()
        .map(|d| d.join(&file))
        .find(|p| p.is_file())
}

/// The Claude Code session id herdr reports for a pane, parsed from `herdr agent get <pane>`
/// JSON: `result.agent.agent_session.value`, accepted only when the agent is `claude` and the
/// reference kind is `id` (herdr reports *paths* for some other agents). Fail-soft: an
/// `agent_not_found` error reply, another agent kind, or an unsafe value is `None`.
///
/// Reply shape verified against herdr 0.9.4:
/// `{"id":"cli:agent:get","result":{"agent":{"agent":"claude","agent_session":{"agent":"claude",
/// "kind":"id","source":"herdr:claude","value":"<uuid>"},…},"type":"agent_info"}}`; a pane
/// without an agent answers `{"error":{"code":"agent_not_found",…}}` with a non-zero exit.
pub fn agent_session_id_from_json(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    let agent = v.pointer("/result/agent")?;
    if agent.get("agent").and_then(Value::as_str) != Some("claude") {
        return None;
    }
    let session = agent.get("agent_session")?;
    if session.get("kind").and_then(Value::as_str) != Some("id") {
        return None;
    }
    session
        .get("value")
        .and_then(Value::as_str)
        .filter(|id| is_safe_session_id(id))
        .map(str::to_owned)
}

/// The user's home directory from the environment (`$HOME`; `%USERPROFILE%` on Windows), which
/// is also what lets the e2e tests point the reader at a fixture home. `None` degrades to "no
/// transcripts found" upstream.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// One discovered session transcript (a `*.jsonl` in the project directory).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
}

/// Every `*.jsonl` transcript in `dir`, most recently modified first — the "newest transcript"
/// default and the session picker's rows. Fail-soft: a missing or unreadable directory (or
/// entry) yields an empty / shorter list.
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionInfo> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(SessionInfo {
                path: e.path(),
                modified: meta.modified().ok()?,
                len: meta.len(),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// How much of a transcript's tail [`session_title`] scans for the last rename.
const TITLE_SCAN: u64 = 64 * 1024;

/// The session's user-given title (the last `custom-title` entry), scanned from a bounded tail
/// window so a large transcript costs one small read. `None` when the session was never
/// renamed, the title lies outside the window, or anything fails — a label nicety, not state.
pub fn session_title(path: &Path) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TITLE_SCAN);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .filter(|l| l.contains("\"customTitle\""))
        .filter_map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            (v.get("type")?.as_str()? == "custom-title")
                .then(|| v.get("customTitle")?.as_str().map(str::to_owned))
                .flatten()
        })
        .next_back()
}

/// The most a single poll reads from one transcript file. Ingestion happens on the input
/// thread, so the work per tick must be bounded: a huge transcript (hundreds of MB after a long
/// agent session) streams in over successive polls — the view fills progressively — instead of
/// freezing the UI for one giant read+parse.
const READ_CHUNK: u64 = 4 * 1024 * 1024;

/// What one file's poll did to the shared member map.
enum FilePoll {
    /// Nothing new (one `stat`).
    Unchanged,
    /// New bytes were applied; the payload says whether the member map changed.
    Applied(bool),
    /// The file shrank or vanished after contributing: the merged map holds entries that no
    /// longer exist in any file, so the whole set must be rebuilt from scratch.
    NeedsRebuild,
}

/// One followed transcript file's read state. The member map lives on [`Follower`], shared by
/// the main transcript and its subagent transcripts.
struct FileFollower {
    path: PathBuf,
    /// The file position everything before which has been consumed (parsed or carried).
    offset: u64,
    /// Bytes of a trailing partial line already read from the file but not yet terminated by a
    /// newline — completed by the next poll's bytes.
    carry: Vec<u8>,
    /// The last observed `(len, mtime)` once fully caught up, so an unchanged file is skipped
    /// with one `stat`. Left unset after a chunk-capped partial read, so the next poll
    /// continues immediately.
    seen: Option<(u64, SystemTime)>,
    /// How one complete line folds into the member map: the transcript grammar
    /// ([`apply_line`]) or the activity log's ([`crate::activity::apply_line`]).
    apply: fn(&mut BTreeMap<PathBuf, Category>, &str) -> bool,
}

impl FileFollower {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_apply(path, apply_line)
    }

    fn with_apply(
        path: impl Into<PathBuf>,
        apply: fn(&mut BTreeMap<PathBuf, Category>, &str) -> bool,
    ) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            carry: Vec::new(),
            seen: None,
            apply,
        }
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.carry.clear();
        self.seen = None;
    }

    /// Poll this one file, folding new lines into `members`.
    ///
    /// Shrink detection is length-only: a transcript truncated AND regrown past the old offset
    /// between two polls is not detected — the resume mis-aligns mid-line and the junk lines
    /// are skipped (fail-soft), healing on the next shrink or session switch. Accepted:
    /// transcripts are append-only in practice, and a rewind rewrite passes through a shorter
    /// length first.
    fn poll(&mut self, members: &mut BTreeMap<PathBuf, Category>) -> FilePoll {
        let Ok(meta) = fs::metadata(&self.path) else {
            // Vanished. If it had contributed anything, the merged set must drop it (rebuild);
            // a file that never contributed is just quiet.
            let had = self.offset > 0 || !self.carry.is_empty();
            self.reset();
            return if had {
                FilePoll::NeedsRebuild
            } else {
                FilePoll::Unchanged
            };
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if self.seen == Some((meta.len(), mtime)) {
            return FilePoll::Unchanged;
        }
        if meta.len() < self.offset {
            self.reset();
            return FilePoll::NeedsRebuild; // shrunk: rewound history must be un-merged
        }
        let (changed, caught_up) = self.consume(members, meta.len());
        if caught_up {
            self.seen = Some((meta.len(), mtime));
        }
        FilePoll::Applied(changed)
    }

    /// Read at most [`READ_CHUNK`] from `offset`, complete lines against `carry`, and apply
    /// each whole line into `members`. Returns `(member map changed, reached EOF)`.
    fn consume(&mut self, members: &mut BTreeMap<PathBuf, Category>, len: u64) -> (bool, bool) {
        let Ok(mut f) = fs::File::open(&self.path) else {
            return (false, true); // unreadable: nothing to do until the world changes
        };
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return (false, true);
        }
        let mut new = Vec::new();
        let Ok(read) = (&mut f).take(READ_CHUNK).read_to_end(&mut new) else {
            return (false, true);
        };
        self.offset += read as u64;
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(&new);
        // Everything up to the last newline is complete lines; the rest waits for the next poll.
        let split = match buf.iter().rposition(|&b| b == b'\n') {
            Some(nl) => nl + 1,
            None => {
                self.carry = buf;
                return (false, self.offset >= len);
            }
        };
        self.carry = buf.split_off(split);
        let mut changed = false;
        for line in String::from_utf8_lossy(&buf).lines() {
            changed |= (self.apply)(members, line);
        }
        (changed, self.offset >= len)
    }
}

/// Incrementally follow one session: the main transcript **and its subagent transcripts**
/// (Task-tool sidechains live in separate files under `<dir>/<session-id>/subagents/*.jsonl` in
/// current Claude Code, so following only the main file would silently omit a subagent's file
/// activity — the membership contract includes it). Every file is read incrementally and
/// bounded per poll; a shrunk or vanished file triggers a clean rebuild of the merged set.
pub struct Follower {
    /// The session's transcript: the follower's identity (what the picker marks as current and
    /// what a re-entry compares against), whether or not it is the followed source.
    transcript: PathBuf,
    source: Source,
    /// Absolute, lexically normalized member paths → strongest category, merged across files.
    members: BTreeMap<PathBuf, Category>,
}

/// What a [`Follower`] reads (CONTEXT.md: the **session activity log** is the live source
/// whenever one exists for the session; the **session transcript** is the history source).
enum Source {
    /// The transcript and its subagent transcripts.
    Transcript {
        main: FileFollower,
        /// Discovered subagent transcripts, each with its own read state.
        subagents: BTreeMap<PathBuf, FileFollower>,
    },
    /// The hook-written activity log — one file, subagent calls included; the transcript is not
    /// consulted at all (strict precedence).
    ActivityLog(FileFollower),
}

impl Follower {
    /// Follow a session through its transcript (and subagent transcripts).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let transcript = path.into();
        Self {
            source: Source::Transcript {
                main: FileFollower::new(transcript.clone()),
                subagents: BTreeMap::new(),
            },
            transcript,
            members: BTreeMap::new(),
        }
    }

    /// Follow a session through its activity log; `transcript` stays the session's identity.
    pub fn with_activity_log(transcript: impl Into<PathBuf>, log: impl Into<PathBuf>) -> Self {
        Self {
            transcript: transcript.into(),
            source: Source::ActivityLog(FileFollower::with_apply(log, crate::activity::apply_line)),
            members: BTreeMap::new(),
        }
    }

    /// The session's transcript path — its identity, even while the activity log is followed.
    pub fn path(&self) -> &Path {
        &self.transcript
    }

    /// Whether the live source (the activity log) is what this follower reads.
    pub fn follows_log(&self) -> bool {
        matches!(self.source, Source::ActivityLog(_))
    }

    /// Where this session's subagent transcripts live: `<dir>/<session-id>/subagents`.
    fn subagents_dir(&self) -> Option<PathBuf> {
        let stem = self.transcript.file_stem()?;
        Some(self.transcript.parent()?.join(stem).join("subagents"))
    }

    /// Read anything appended since the last poll — in the main transcript or any subagent
    /// transcript — and fold it in. Returns `true` when the member set changed (the caller's
    /// re-synthesize signal). An idle session costs one `stat` per followed file plus one
    /// (usually missing) subagents-directory listing.
    pub fn poll(&mut self) -> bool {
        // Discover new subagent transcripts; existing entries keep their read state.
        let subagents_dir = self.subagents_dir();
        if let Source::Transcript { subagents, .. } = &mut self.source
            && let Some(dir) = subagents_dir
        {
            for info in list_sessions(&dir) {
                subagents
                    .entry(info.path.clone())
                    .or_insert_with(|| FileFollower::new(info.path));
            }
        }
        let mut changed = false;
        let mut rebuild = false;
        for f in self.source.files_mut() {
            match f.poll(&mut self.members) {
                FilePoll::Unchanged => {}
                FilePoll::Applied(c) => changed |= c,
                FilePoll::NeedsRebuild => rebuild = true,
            }
        }
        if rebuild {
            changed = self.rebuild();
        }
        changed
    }

    /// Rebuild the merged member map from scratch: a followed file shrank or vanished, so the
    /// map may hold entries no surviving file records. Vanished subagent files are dropped;
    /// every survivor re-reads from the start (chunk-bounded — a large history streams back in
    /// over the following polls). Change is judged against the old map, so a rewrite that
    /// reproduces the same members reports "nothing changed".
    fn rebuild(&mut self) -> bool {
        let before = std::mem::take(&mut self.members);
        if let Source::Transcript { subagents, .. } = &mut self.source {
            subagents.retain(|path, _| path.is_file());
        }
        for f in self.source.files_mut() {
            f.reset();
            f.poll(&mut self.members); // a second shrink mid-rebuild is impossible: offset is 0
        }
        before != self.members
    }
}

impl Source {
    /// Every followed file, main transcript first.
    fn files_mut(&mut self) -> Box<dyn Iterator<Item = &mut FileFollower> + '_> {
        match self {
            Source::Transcript { main, subagents } => {
                Box::new(std::iter::once(main).chain(subagents.values_mut()))
            }
            Source::ActivityLog(log) => Box::new(std::iter::once(log)),
        }
    }
}

/// Fold one transcript line into the member map. Only the deterministic signals count:
///
/// - **Write results** — `toolUseResult: {"type": "create" | "update", "filePath": …}` —
///   the explicit created/updated discriminator.
/// - **Edit results** — `toolUseResult: {"filePath": …, "oldString": …, …}` → updated.
/// - **Read results** — `toolUseResult: {"file": {"filePath": …}, …}` → mentioned.
/// - **User `@`-mentions** — an `attachment` entry of type `file` / `already_read_file`
///   with a `filename` → mentioned.
/// - **Bash edit records** — `toolUseResult: {"bashEditDiff": {"changedFiles": […],
///   "files": [{"filePath": …, "created": …}, …]}}`, Claude Code's own before/after record of the
///   files a Bash command changed (the shape it documents for its PostToolUse hook payload;
///   recorded in auto mode or with `bashEditDiffEnabled`): every path → updated, a `created`
///   entry → created. Best-effort by Claude Code's own account, so it narrows the shell gap
///   rather than closing it; the session activity log is the complete live source.
///
/// Anything else — malformed JSON, unknown shapes, error results (plain strings), shell
/// command output — is skipped without complaint (ADR-0011's fail-soft posture).
fn apply_line(members: &mut BTreeMap<PathBuf, Category>, line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let mut changed = false;
    if let Some(r) = v.get("toolUseResult").filter(|r| r.is_object()) {
        let file_path = r.get("filePath").and_then(Value::as_str);
        match (r.get("type").and_then(Value::as_str), file_path) {
            (Some("create"), Some(p)) => changed |= record(members, p, Category::Created),
            (Some("update"), Some(p)) => changed |= record(members, p, Category::Updated),
            _ => {
                if r.get("oldString").is_some()
                    && let Some(p) = file_path
                {
                    changed |= record(members, p, Category::Updated);
                }
            }
        }
        if let Some(p) = r.pointer("/file/filePath").and_then(Value::as_str) {
            changed |= record(members, p, Category::Mentioned);
        }
        if let Some(diff) = r.get("bashEditDiff").filter(|d| d.is_object()) {
            changed |= apply_bash_edit_diff(members, diff);
        }
    }
    if v.get("type").and_then(Value::as_str) == Some("attachment")
        && let Some(a) = v.get("attachment")
        && matches!(
            a.get("type").and_then(Value::as_str),
            Some("file" | "already_read_file")
        )
        && let Some(p) = a.get("filename").and_then(Value::as_str)
    {
        changed |= record(members, p, Category::Mentioned);
    }
    changed
}

/// Fold Claude Code's Bash edit record into the member map. `changedFiles` lists every changed
/// path (absolute, capped by Claude Code at 200); `files` carries the few it also diffed, each
/// with `filePath` and an optional `created` flag — the only created/updated discriminator the
/// record offers, so a `files` entry marked created promotes its path. Older records carry only
/// `files`; both shapes are read. `unavailable` / `skipped` records list what they could, which
/// is still real, so nothing is withheld on those flags.
fn apply_bash_edit_diff(members: &mut BTreeMap<PathBuf, Category>, diff: &Value) -> bool {
    let mut changed = false;
    for p in diff
        .get("changedFiles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        changed |= record(members, p, Category::Updated);
    }
    for file in diff
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(p) = file.get("filePath").and_then(Value::as_str) else {
            continue;
        };
        let cat = if file.get("created").and_then(Value::as_bool) == Some(true) {
            Category::Created
        } else {
            Category::Updated
        };
        changed |= record(members, p, cat);
    }
    changed
}

/// Admit one path at `cat`, keeping the strongest category seen (created ≻ updated ≻
/// mentioned). Relative or non-normalizable paths are skipped: transcript tool paths are
/// absolute, so anything else is not trustworthy enough to display.
pub(crate) fn record(members: &mut BTreeMap<PathBuf, Category>, path: &str, cat: Category) -> bool {
    let p = Path::new(path);
    if !p.is_absolute() {
        return false;
    }
    let Some(norm) = lexically_normalize(p) else {
        return false;
    };
    match members.get_mut(&norm) {
        Some(existing) if *existing >= cat => false,
        Some(existing) => {
            *existing = cat;
            true
        }
        None => {
            members.insert(norm, cat);
            true
        }
    }
}

impl Follower {
    /// Project the members around `root`: members under it become root-relative `in_root` keys
    /// (composing with the tree's changed-set conventions), the rest stay absolute in
    /// `outside`. The root itself (an exact match) is never a member row.
    pub fn set_for(&self, root: &Path) -> SessionSet {
        let root = lexically_normalize(root).unwrap_or_else(|| root.to_path_buf());
        let mut set = SessionSet::default();
        for (path, cat) in &self.members {
            match path.strip_prefix(&root) {
                Ok(rel) if !rel.as_os_str().is_empty() => {
                    set.in_root.insert(rel.to_path_buf(), *cat);
                }
                _ if path != &root => {
                    set.outside.insert(path.clone(), *cat);
                }
                _ => {}
            }
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_maps_every_non_alphanumeric_byte_to_a_dash() {
        assert_eq!(
            project_slug(Path::new("/Volumes/workplace/OpenSource/herdr-file-viewer")),
            "-Volumes-workplace-OpenSource-herdr-file-viewer"
        );
        // A dot doubles the dash (verified against a real ~/.claude/projects entry), and
        // underscores map too — every non-alphanumeric byte, not just separators.
        assert_eq!(
            project_slug(Path::new("/Users/x/.claude/my_pkg")),
            "-Users-x--claude-my-pkg"
        );
        // Astral characters are two UTF-16 code units → two dashes, matching Claude Code's
        // per-code-unit JS replace.
        assert_eq!(project_slug(Path::new("/r/😀x")), "-r---x");
    }

    #[test]
    fn projects_dir_joins_home_claude_projects_slug() {
        assert_eq!(
            projects_dir(Path::new("/home/u"), Path::new("/repo/x")),
            Path::new("/home/u/.claude/projects/-repo-x")
        );
    }

    fn fed(lines: &[&str]) -> Follower {
        let mut f = Follower::new("/nowhere.jsonl");
        for l in lines {
            apply_line(&mut f.members, l);
        }
        f
    }

    #[test]
    fn write_results_discriminate_created_from_updated() {
        let f = fed(&[
            r#"{"toolUseResult":{"type":"create","filePath":"/r/new.rs","content":"x"}}"#,
            r#"{"toolUseResult":{"type":"update","filePath":"/r/old.rs","content":"y"}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(
            set.in_root.get(Path::new("new.rs")),
            Some(&Category::Created)
        );
        assert_eq!(
            set.in_root.get(Path::new("old.rs")),
            Some(&Category::Updated)
        );
    }

    #[test]
    fn edit_results_count_as_updated_and_read_results_as_mentioned() {
        let f = fed(&[
            r#"{"toolUseResult":{"filePath":"/r/a.rs","oldString":"x","newString":"y","structuredPatch":[]}}"#,
            r#"{"toolUseResult":{"type":"text","file":{"filePath":"/r/b.rs","content":"…"}}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(set.in_root.get(Path::new("a.rs")), Some(&Category::Updated));
        assert_eq!(
            set.in_root.get(Path::new("b.rs")),
            Some(&Category::Mentioned)
        );
    }

    #[test]
    fn at_mention_attachments_count_as_mentioned() {
        let f = fed(&[
            r#"{"type":"attachment","attachment":{"type":"file","filename":"/r/doc.md","displayPath":"doc.md"}}"#,
            r#"{"type":"attachment","attachment":{"type":"already_read_file","filename":"/r/dup.md","displayPath":"dup.md"}}"#,
            // Directory mentions and hook noise attachments are not file members.
            r#"{"type":"attachment","attachment":{"type":"directory","path":"/r/src","displayPath":"src"}}"#,
            r#"{"type":"attachment","attachment":{"type":"skill_listing"}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(
            set.in_root.get(Path::new("doc.md")),
            Some(&Category::Mentioned)
        );
        assert_eq!(
            set.in_root.get(Path::new("dup.md")),
            Some(&Category::Mentioned)
        );
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn strongest_category_wins_regardless_of_order() {
        // read → edit → create-over: created is terminal.
        let f = fed(&[
            r#"{"toolUseResult":{"type":"text","file":{"filePath":"/r/f.rs"}}}"#,
            r#"{"toolUseResult":{"filePath":"/r/f.rs","oldString":"a"}}"#,
            r#"{"toolUseResult":{"type":"create","filePath":"/r/f.rs"}}"#,
            // …and a later read never demotes it.
            r#"{"toolUseResult":{"type":"text","file":{"filePath":"/r/f.rs"}}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(set.in_root.get(Path::new("f.rs")), Some(&Category::Created));
    }

    #[test]
    fn junk_error_results_and_relative_paths_are_skipped() {
        let mut f = Follower::new("/nowhere.jsonl");
        assert!(!apply_line(&mut f.members, "not json at all"));
        assert!(!apply_line(
            &mut f.members,
            r#"{"toolUseResult":"Error: denied"}"#
        ));
        assert!(!apply_line(
            &mut f.members,
            r#"{"toolUseResult":{"stdout":"…","stderr":""}}"#
        ));
        assert!(!apply_line(
            &mut f.members,
            r#"{"toolUseResult":{"type":"create","filePath":"relative.rs"}}"#
        ));
        assert!(f.set_for(Path::new("/r")).is_empty());
    }

    #[test]
    fn set_partitions_members_around_the_root() {
        let f = fed(&[
            r#"{"toolUseResult":{"type":"create","filePath":"/r/src/new.rs"}}"#,
            r#"{"toolUseResult":{"type":"text","file":{"filePath":"/etc/hosts"}}}"#,
            r#"{"toolUseResult":{"filePath":"/home/u/.claude/CLAUDE.md","oldString":"x"}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(set.in_root.len(), 1);
        assert_eq!(
            set.outside.get(Path::new("/etc/hosts")),
            Some(&Category::Mentioned)
        );
        assert_eq!(
            set.outside.get(Path::new("/home/u/.claude/CLAUDE.md")),
            Some(&Category::Updated)
        );
        // Not-under-root is decided component-wise, not by string prefix: /r2 is outside /r.
        let f2 = fed(&[r#"{"toolUseResult":{"type":"create","filePath":"/r2/x.rs"}}"#]);
        assert_eq!(f2.set_for(Path::new("/r")).outside.len(), 1);
    }

    #[test]
    fn dot_segments_normalize_before_partitioning() {
        let f = fed(&[r#"{"toolUseResult":{"type":"create","filePath":"/r/src/../src/./a.rs"}}"#]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(
            set.in_root.get(Path::new("src/a.rs")),
            Some(&Category::Created)
        );
    }

    #[test]
    fn category_glyphs_match_the_agreed_markers() {
        assert_eq!(Category::Created.glyph(), '+');
        assert_eq!(Category::Updated.glyph(), '~');
        assert_eq!(Category::Mentioned.glyph(), '·');
    }

    #[test]
    fn bash_edit_records_count_as_updated_or_created() {
        let f = fed(&[
            // The documented hook shape: every changed path, plus diffs for a few of them.
            r#"{"toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"bashEditDiff":{"changedFiles":["/r/a.rs","/r/b.rs","/r/new.rs"],"files":[{"filePath":"/r/a.rs","hunks":[]},{"filePath":"/r/new.rs","created":true,"hunks":[]}],"moreFiles":0}}}"#,
            // The older files-only shape (observed in real transcripts).
            r#"{"toolUseResult":{"stdout":"","stderr":"","bashEditDiff":{"files":[{"filePath":"/tmp/out/gen.rs","hunks":[{"oldStart":1,"oldLines":0,"newStart":1,"newLines":2,"lines":["+x","+y"]}]}]}}}"#,
            // Flags never withhold what was listed; junk entries are skipped.
            r#"{"toolUseResult":{"stdout":"","bashEditDiff":{"unavailable":true,"changedFiles":["/r/c.rs","relative.rs",7],"files":[{"hunks":[]}]}}}"#,
        ]);
        let set = f.set_for(Path::new("/r"));
        assert_eq!(set.in_root.get(Path::new("a.rs")), Some(&Category::Updated));
        assert_eq!(set.in_root.get(Path::new("b.rs")), Some(&Category::Updated));
        assert_eq!(set.in_root.get(Path::new("c.rs")), Some(&Category::Updated));
        assert_eq!(
            set.in_root.get(Path::new("new.rs")),
            Some(&Category::Created),
            "the created flag is the only discriminator the record offers"
        );
        assert_eq!(
            set.outside.get(Path::new("/tmp/out/gen.rs")),
            Some(&Category::Updated)
        );
        assert_eq!(set.len(), 5);
        // A plain Bash result (no record) still contributes nothing.
        let mut g = Follower::new("/nowhere.jsonl");
        assert!(!apply_line(
            &mut g.members,
            r#"{"toolUseResult":{"stdout":"ok","stderr":"","bashEditDiff":"not-an-object"}}"#
        ));
    }

    #[test]
    fn session_ids_are_uuid_shaped_tokens_only() {
        assert!(is_safe_session_id("3a744a71-5cce-481b-8107-db5bf2f355d9"));
        assert!(is_safe_session_id("abc"));
        assert!(!is_safe_session_id(""));
        assert!(!is_safe_session_id("../../etc/passwd"));
        assert!(!is_safe_session_id("a/b"));
        assert!(!is_safe_session_id("a.jsonl"));
        assert!(!is_safe_session_id("--flag"));
    }

    /// The reply shape verified against herdr 0.9.4 (`herdr agent get <pane>`).
    const AGENT_GET_CLAUDE: &str = r#"{"id":"cli:agent:get","result":{"agent":{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"3a744a71-5cce-481b-8107-db5bf2f355d9"},"agent_status":"working","cwd":"/r","focused":true,"name":"x","pane_id":"w1:p1Q","revision":1,"tab_id":"w1:tF","workspace_id":"w1"},"type":"agent_info"}}"#;

    #[test]
    fn herdr_agent_reply_yields_the_claude_session_id() {
        assert_eq!(
            agent_session_id_from_json(AGENT_GET_CLAUDE).as_deref(),
            Some("3a744a71-5cce-481b-8107-db5bf2f355d9")
        );
    }

    #[test]
    fn herdr_agent_reply_without_a_claude_id_session_is_ignored() {
        // A pane without an agent (herdr exits non-zero; the body is still parsed defensively).
        assert_eq!(
            agent_session_id_from_json(
                r#"{"error":{"code":"agent_not_found","message":"agent target w1:p34 not found"},"id":"cli:agent:get"}"#
            ),
            None
        );
        // Another agent kind: its session is not a Claude Code transcript.
        assert_eq!(
            agent_session_id_from_json(
                &AGENT_GET_CLAUDE.replace("\"agent\":\"claude\"", "\"agent\":\"codex\"")
            ),
            None
        );
        // A path-kind reference is not an id.
        assert_eq!(
            agent_session_id_from_json(
                &AGENT_GET_CLAUDE.replace("\"kind\":\"id\"", "\"kind\":\"path\"")
            ),
            None
        );
        // A hostile value never becomes a file name.
        assert_eq!(
            agent_session_id_from_json(
                &AGENT_GET_CLAUDE.replace("3a744a71-5cce-481b-8107-db5bf2f355d9", "../../.bashrc")
            ),
            None
        );
        assert_eq!(agent_session_id_from_json("not json"), None);
    }
}
