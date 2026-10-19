//! Session view + session picker — controller-level behavior over a fake `$HOME` transcript
//! store: the `s` toggle, mutual exclusivity with the git filters, live-follow through
//! `poll()`, the `S` picker's explicit hold, refresh's newest re-resolution, and the startup
//! config chain. Tree synthesis lives in `session_view.rs`; parsing in `session_transcript.rs`.

mod common;

use common::TempDir;
use herdr_file_viewer::activity;
use herdr_file_viewer::controller::{
    Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::herdr::HerdrCli;
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::session::projects_dir;
use herdr_file_viewer::view_policy::ViewMode;
use ratatui::text::Text;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

// One fake `$HOME` for the whole test binary, set before any test reads it (every test goes
// through `fake_home()` first, and `OnceLock` orders the write before every later reader).
// Tests share it safely because the transcript store is keyed by each test's UNIQUE root slug.
static HOME: OnceLock<TempDir> = OnceLock::new();
fn fake_home() -> &'static Path {
    HOME.get_or_init(|| {
        let dir = TempDir::new();
        // Edition 2024 marks env mutation unsafe (process-global). This is the only write, and
        // it happens-before every read via the OnceLock barrier.
        unsafe { std::env::set_var("HOME", dir.path()) };
        // The activity log lives under the state dir: point it into the same fake home so a
        // developer's real `$XDG_STATE_HOME` is never read or written by these tests.
        unsafe { std::env::set_var("XDG_STATE_HOME", dir.path().join("state")) };
        dir
    })
    .path()
}

struct StubGit {
    changed: BTreeMap<PathBuf, Status>,
}
impl GitService for StubGit {
    fn status(&self) -> BTreeMap<PathBuf, Status> {
        self.changed.clone()
    }
    fn changed_set(&self, _baseline: Baseline) -> BTreeMap<PathBuf, Status> {
        self.changed.clone()
    }
    fn diff(&self, _rel: &Path, _baseline: Baseline, _full: bool) -> String {
        String::new()
    }
    fn diff_directory(&self, _rel: &Path, _baseline: Baseline) -> String {
        String::new()
    }
}

struct StubContent;
impl ContentProvider for StubContent {
    fn render(&self, _path: &Path, _mode: ViewMode, _raw_diff: Option<&str>) -> RenderResult {
        RenderResult {
            content: Text::raw("stub-content"),
            notices: Vec::new(),
            source: None,
        }
    }
}

struct StubEditor;
impl EditorHandoff for StubEditor {
    fn open(&mut self, _file: &Path) -> EditorOutcome {
        EditorOutcome::NoTakeover
    }
}

fn controller(root: &Path, is_git_repo: bool, changed: BTreeMap<PathBuf, Status>) -> Controller {
    let git: Arc<dyn GitService> = Arc::new(StubGit { changed });
    Controller::new(
        common::resolved(root.to_path_buf(), is_git_repo),
        Baseline::Head,
        Components {
            providers: Box::new(move |_resolved| RootProviders {
                git: Arc::clone(&git),
                content: Box::new(StubContent),
            }),
            editor: Box::new(StubEditor),
            clipboard: Box::new(common::RecordingClipboard::default()),
            renderers: None,
        },
    )
}

fn create_line(path: &Path) -> String {
    format!(
        "{{\"toolUseResult\":{{\"type\":\"create\",\"filePath\":\"{}\"}}}}\n",
        path.display()
    )
}

/// Write a transcript named `name` for `root` into the fake home's project store.
fn write_transcript(root: &Path, name: &str, body: &str) -> PathBuf {
    let dir = projects_dir(fake_home(), root);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    path
}

/// The file names of the session view's visible file rows.
fn file_rows(ctrl: &Controller) -> Vec<String> {
    ctrl.tree()
        .visible_nodes()
        .iter()
        .filter(|n| n.kind == herdr_file_viewer::tree::NodeKind::File)
        .map(|n| n.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn toggle_enters_the_session_view_and_back() {
    let root = TempDir::new();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/a.rs"), "x").unwrap();
    fs::write(root.path().join("unrelated.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "s1.jsonl",
        &create_line(&root.path().join("src/a.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert!(ctrl.tree().session_view());
    assert_eq!(
        file_rows(&ctrl),
        ["a.rs"],
        "members only, not the full tree"
    );

    ctrl.handle(Intent::ToggleSessionView);
    assert!(!ctrl.tree().session_view());
    assert!(
        file_rows(&ctrl).contains(&"unrelated.rs".to_string()),
        "leaving restores the full tree"
    );
}

#[test]
fn session_view_and_git_filters_displace_each_other() {
    let root = TempDir::new();
    fs::write(root.path().join("a.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "s1.jsonl",
        &create_line(&root.path().join("a.rs")),
    );
    let changed: BTreeMap<PathBuf, Status> = [(PathBuf::from("a.rs"), Status::Modified)]
        .into_iter()
        .collect();

    let mut ctrl = controller(root.path(), true, changed);
    ctrl.handle(Intent::ToggleSessionView);
    assert!(ctrl.tree().session_view());

    // Entering changed-only leaves the session view…
    ctrl.handle(Intent::ToggleChangedOnly);
    assert!(!ctrl.tree().session_view());
    assert!(ctrl.changed_only());
    // …and entering the session view leaves changed-only.
    ctrl.handle(Intent::ToggleSessionView);
    assert!(ctrl.tree().session_view());
    assert!(!ctrl.changed_only());
    // Status mode displaces it too.
    ctrl.handle(Intent::ToggleStatusMode);
    assert!(!ctrl.tree().session_view());
    assert!(ctrl.status_mode());
}

#[test]
fn live_follow_picks_up_appends_through_poll() {
    let root = TempDir::new();
    fs::write(root.path().join("a.rs"), "x").unwrap();
    fs::write(root.path().join("b.rs"), "x").unwrap();
    let transcript = write_transcript(
        root.path(),
        "s1.jsonl",
        &create_line(&root.path().join("a.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["a.rs"]);

    let mut fh = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    fh.write_all(create_line(&root.path().join("b.rs")).as_bytes())
        .unwrap();
    drop(fh);

    // Entering reset the follow throttle, so the next poll ingests the append immediately.
    let effects = ctrl.poll();
    assert!(effects.is_some(), "a set change must request a redraw");
    assert_eq!(file_rows(&ctrl), ["a.rs", "b.rs"]);
}

#[test]
fn no_transcript_shows_an_empty_view_with_a_notice() {
    // Route through the shared fake home like every other test: this test READS $HOME (via
    // home_dir() inside the toggle), so it must order itself after the OnceLock's set_var —
    // and must not depend on the machine's real ~/.claude store.
    let _ = fake_home();
    let root = TempDir::new();
    fs::write(root.path().join("a.rs"), "x").unwrap();

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert!(ctrl.tree().session_view());
    assert!(file_rows(&ctrl).is_empty());
    assert!(
        ctrl.action_notice()
            .is_some_and(|n| n.contains("No Claude Code session")),
        "notice: {:?}",
        ctrl.action_notice()
    );
}

#[test]
fn picker_choice_holds_against_a_newer_session() {
    let root = TempDir::new();
    fs::write(root.path().join("old.rs"), "x").unwrap();
    fs::write(root.path().join("new.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "older.jsonl",
        &create_line(&root.path().join("old.rs")),
    );
    std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtimes
    write_transcript(
        root.path(),
        "newer.jsonl",
        &create_line(&root.path().join("new.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["new.rs"], "auto mode follows the newest");

    // Open the picker: newest first, the presented one marked and pre-selected.
    ctrl.handle(Intent::OpenSessionPicker);
    let view = ctrl.view_state();
    let picker = view.session_picker.expect("picker open");
    assert_eq!(picker.rows.len(), 2);
    assert!(picker.rows[0].is_current);
    assert_eq!(picker.cursor, 0);

    // Choose the older session.
    ctrl.handle(Intent::NavDown);
    ctrl.handle(Intent::Activate);
    assert!(ctrl.view_state().session_picker.is_none(), "picker closed");
    assert_eq!(file_rows(&ctrl), ["old.rs"]);

    // The explicit choice holds: even with the newer transcript still newer, a refresh's
    // immediate re-check must NOT switch back.
    ctrl.handle(Intent::Refresh);
    assert_eq!(file_rows(&ctrl), ["old.rs"]);
}

#[test]
fn refresh_re_resolves_the_newest_session_in_auto_mode() {
    let root = TempDir::new();
    fs::write(root.path().join("first.rs"), "x").unwrap();
    fs::write(root.path().join("second.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "a.jsonl",
        &create_line(&root.path().join("first.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["first.rs"]);

    // A brand-new session appears (a fresh `claude` in this root): auto mode follows it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_transcript(
        root.path(),
        "b.jsonl",
        &create_line(&root.path().join("second.rs")),
    );
    ctrl.handle(Intent::Refresh);
    assert_eq!(file_rows(&ctrl), ["second.rs"]);
}

#[test]
fn startup_config_enters_the_session_view_before_the_first_frame() {
    let root = TempDir::new();
    fs::write(root.path().join("a.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "s1.jsonl",
        &create_line(&root.path().join("a.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.startup_session_view();
    assert!(ctrl.tree().session_view());
    assert_eq!(file_rows(&ctrl), ["a.rs"]);
    // Idempotent: applying it again (or after a manual toggle) never double-enters.
    ctrl.startup_session_view();
    assert!(ctrl.tree().session_view());
}

#[test]
fn session_jump_cycles_member_files() {
    let root = TempDir::new();
    fs::create_dir_all(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/a.rs"), "x").unwrap();
    fs::write(root.path().join("z.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "s1.jsonl",
        &format!(
            "{}{}",
            create_line(&root.path().join("src/a.rs")),
            create_line(&root.path().join("z.rs")),
        ),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    // `]` steps member → member (skipping the dir rows), wrapping with a notice.
    ctrl.handle(Intent::NextChanged);
    let sel = ctrl.tree().selected().unwrap();
    assert_eq!(sel.path.file_name().unwrap(), "a.rs");
    ctrl.handle(Intent::NextChanged);
    assert_eq!(
        ctrl.tree().selected().unwrap().path.file_name().unwrap(),
        "z.rs"
    );
    ctrl.handle(Intent::NextChanged);
    assert_eq!(
        ctrl.tree().selected().unwrap().path.file_name().unwrap(),
        "a.rs"
    );
    assert!(
        ctrl.action_notice()
            .is_some_and(|n| n.contains("wrapped to the first")),
        "wrap notice expected"
    );
}

/// A `herdr agent get <pane>` fake answering one fixed reply. The argv is pinned here so a herdr
/// CLI change cannot silently break the lookup (verified against herdr 0.9.4).
struct AgentGetFake {
    reply: Option<String>,
}
impl HerdrCli for AgentGetFake {
    fn run_json(&self, args: &[&str]) -> std::io::Result<String> {
        assert_eq!(args, ["agent", "get", "w1:p7"]);
        self.reply
            .clone()
            .ok_or_else(|| std::io::Error::other("herdr exited non-zero"))
    }
}

/// herdr's reply for a Claude pane whose session id is `uuid` (shape verified against 0.9.4).
fn agent_get_reply(uuid: &str) -> String {
    format!(
        r#"{{"id":"cli:agent:get","result":{{"agent":{{"agent":"claude","agent_session":{{"agent":"claude","kind":"id","source":"herdr:claude","value":"{uuid}"}},"agent_status":"idle","pane_id":"w1:p7"}},"type":"agent_info"}}}}"#
    )
}

/// Two transcripts for the root: the herdr-named one (older) and a sibling pane's (newer).
fn two_sessions(root: &Path, uuid: &str) {
    fs::write(root.join("mine.rs"), "x").unwrap();
    fs::write(root.join("sibling.rs"), "x").unwrap();
    write_transcript(
        root,
        &format!("{uuid}.jsonl"),
        &create_line(&root.join("mine.rs")),
    );
    std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtimes
    write_transcript(
        root,
        "sibling.jsonl",
        &create_line(&root.join("sibling.rs")),
    );
}

fn with_herdr(root: &Path, reply: Option<String>) -> Controller {
    let mut ctrl = controller(root, false, BTreeMap::new());
    ctrl.set_host(Box::new(AgentGetFake { reply }), None);
    ctrl.set_focused_pane_id(Some("w1:p7".to_string()));
    ctrl
}

#[test]
fn herdr_named_session_wins_over_the_newest_transcript() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5a0";
    two_sessions(root.path(), uuid);

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply(uuid)));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(
        file_rows(&ctrl),
        ["mine.rs"],
        "the accompanied pane's session, not the newest transcript"
    );
    // The 3 s rescan re-asks herdr: the sibling stays out even though it is newer.
    ctrl.handle(Intent::Refresh);
    assert_eq!(file_rows(&ctrl), ["mine.rs"]);
}

#[test]
fn herdr_failure_falls_back_to_the_newest_transcript() {
    let root = TempDir::new();
    two_sessions(root.path(), "7819f839-922e-49eb-a080-63b3ab7ae5a1");

    // `agent_not_found` (a pane without an agent) exits non-zero: the seam reports an error.
    let mut ctrl = with_herdr(root.path(), None);
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["sibling.rs"]);
}

#[test]
fn herdr_named_session_under_another_slug_is_still_found() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5a2";
    fs::write(root.path().join("mine.rs"), "x").unwrap();
    fs::write(root.path().join("sibling.rs"), "x").unwrap();
    // Claude Code was started in a subdirectory: its transcript lives under that slug.
    write_transcript(
        &root.path().join("sub"),
        &format!("{uuid}.jsonl"),
        &create_line(&root.path().join("mine.rs")),
    );
    write_transcript(
        root.path(),
        "sibling.jsonl",
        &create_line(&root.path().join("sibling.rs")),
    );

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply(uuid)));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["mine.rs"]);
}

#[test]
fn unsafe_session_id_from_herdr_is_refused() {
    let root = TempDir::new();
    two_sessions(root.path(), "7819f839-922e-49eb-a080-63b3ab7ae5a3");

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply("../../.bashrc")));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(
        file_rows(&ctrl),
        ["sibling.rs"],
        "fell back to the newest transcript"
    );
}

#[test]
fn without_a_pane_id_the_newest_transcript_rule_applies() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5a4";
    two_sessions(root.path(), uuid);

    // herdr would name the older session, but outside herdr no pane id reaches the controller.
    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.set_host(
        Box::new(AgentGetFake {
            reply: Some(agent_get_reply(uuid)),
        }),
        None,
    );
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["sibling.rs"]);
}

/// Write a session activity log for `uuid` into the fake home's state dir.
fn write_activity_log(uuid: &str, body: &str) -> PathBuf {
    fake_home();
    let dir = activity::log_dir().expect("the fake home provides a state dir");
    fs::create_dir_all(&dir).unwrap();
    let path = activity::log_path(&dir, uuid).unwrap();
    fs::write(&path, body).unwrap();
    path
}

fn log_line(activity: &str, path: &Path) -> String {
    format!(
        "{{\"kind\":\"file\",\"ts\":1,\"activity\":\"{activity}\",\"path\":\"{}\",\"evidence\":\"scan\",\"tool\":\"Bash\"}}\n",
        path.display()
    )
}

#[test]
fn an_activity_log_takes_precedence_over_the_transcript() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5b0";
    two_sessions(root.path(), uuid);
    fs::write(root.path().join("logged.rs"), "x").unwrap();
    write_activity_log(uuid, &log_line("create", &root.path().join("logged.rs")));

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply(uuid)));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(
        file_rows(&ctrl),
        ["logged.rs"],
        "the log is the only source for its session: the transcript's member is not merged in"
    );
}

#[test]
fn a_log_appearing_later_switches_the_view_to_it() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5b1";
    two_sessions(root.path(), uuid);
    fs::write(root.path().join("logged.rs"), "x").unwrap();

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply(uuid)));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(
        file_rows(&ctrl),
        ["mine.rs"],
        "transcript first: no log yet"
    );

    // The hook gets installed mid-session and opens the log: the rescan switches sources.
    write_activity_log(uuid, &log_line("update", &root.path().join("logged.rs")));
    ctrl.handle(Intent::Refresh);
    assert_eq!(file_rows(&ctrl), ["logged.rs"]);
}

#[test]
fn a_brand_new_session_with_only_a_log_is_followed() {
    let root = TempDir::new();
    let uuid = "7819f839-922e-49eb-a080-63b3ab7ae5b2";
    fs::write(root.path().join("fresh.rs"), "x").unwrap();
    fs::write(root.path().join("sibling.rs"), "x").unwrap();
    // Only a sibling's transcript exists on disk; the named session has written no transcript
    // yet, but the hook already recorded its first edit.
    write_transcript(
        root.path(),
        "sibling.jsonl",
        &create_line(&root.path().join("sibling.rs")),
    );
    write_activity_log(uuid, &log_line("create", &root.path().join("fresh.rs")));

    let mut ctrl = with_herdr(root.path(), Some(agent_get_reply(uuid)));
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["fresh.rs"]);
}

#[test]
fn a_picked_session_uses_its_log_when_present() {
    let root = TempDir::new();
    fs::write(root.path().join("old.rs"), "x").unwrap();
    fs::write(root.path().join("new.rs"), "x").unwrap();
    fs::write(root.path().join("logged.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "aaaa0000.jsonl",
        &create_line(&root.path().join("old.rs")),
    );
    write_activity_log(
        "aaaa0000",
        &log_line("create", &root.path().join("logged.rs")),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_transcript(
        root.path(),
        "bbbb0000.jsonl",
        &create_line(&root.path().join("new.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.handle(Intent::ToggleSessionView);
    assert_eq!(file_rows(&ctrl), ["new.rs"]);
    ctrl.handle(Intent::OpenSessionPicker);
    ctrl.handle(Intent::NavDown);
    ctrl.handle(Intent::Activate);
    assert_eq!(
        file_rows(&ctrl),
        ["logged.rs"],
        "the pick resolves to that session's log"
    );
}

#[test]
fn a_missing_hook_is_announced_when_entering_the_session_view() {
    let root = TempDir::new();
    fs::write(root.path().join("a.rs"), "x").unwrap();
    write_transcript(
        root.path(),
        "cccc0000.jsonl",
        &create_line(&root.path().join("a.rs")),
    );

    let mut ctrl = controller(root.path(), false, BTreeMap::new());
    ctrl.set_session_hook_status(Some(false));
    ctrl.handle(Intent::ToggleSessionView);
    let notice = ctrl.action_notice().unwrap_or_default();
    assert!(
        notice.contains("hook install"),
        "names the remedy: {notice:?}"
    );

    let mut quiet = controller(root.path(), false, BTreeMap::new());
    quiet.set_session_hook_status(Some(true));
    quiet.handle(Intent::ToggleSessionView);
    assert_eq!(quiet.action_notice(), None);
}
