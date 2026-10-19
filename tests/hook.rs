//! The session hook's filesystem side: the Bash-call stamps and the disk scan that catch shell
//! edits the command text never names (CONTEXT.md "touched"). The payload → record mapping and
//! the settings editing are pure and tested inline in `src/hook.rs`.

mod common;

use common::TempDir;
use herdr_file_viewer::hook::{scan_changed_since, take_stamp, write_stamp};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime};

fn rel(root: &Path, found: &[(PathBuf, bool)]) -> Vec<(String, bool)> {
    found
        .iter()
        .map(|(p, created)| {
            (
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/"),
                *created,
            )
        })
        .collect()
}

#[test]
fn scan_reports_files_changed_after_the_stamp_and_skips_build_trees() {
    let root = TempDir::new();
    let r = root.path();
    fs::write(r.join("old.rs"), "x").unwrap();
    sleep(Duration::from_millis(30)); // mtime resolution slack
    let since = SystemTime::now();
    sleep(Duration::from_millis(30));
    for dir in [
        "sub",
        "target",
        "node_modules",
        "build",
        ".hidden",
        "a/b/c/d",
    ] {
        fs::create_dir_all(r.join(dir)).unwrap();
    }
    fs::write(r.join("new.rs"), "x").unwrap();
    fs::write(r.join("sub/deep.rs"), "x").unwrap();
    fs::write(r.join(".hidden/dot.rs"), "x").unwrap(); // dotfiles count
    fs::write(r.join("target/junk.rs"), "x").unwrap(); // pruned
    fs::write(r.join("node_modules/x.js"), "x").unwrap(); // pruned
    fs::write(r.join("build/out.o"), "x").unwrap(); // pruned
    fs::write(r.join("a/b/c/d/too_deep.rs"), "x").unwrap(); // beyond depth 4 outside a repo

    let found = rel(r, &scan_changed_since(r, since));
    assert_eq!(
        found,
        [
            (".hidden/dot.rs".to_string(), false),
            ("new.rs".to_string(), false),
            ("sub/deep.rs".to_string(), false),
        ],
        "sorted; outside a repository nothing is 'created', only changed"
    );
}

#[test]
fn scan_inside_a_repo_marks_untracked_files_created_and_honors_gitignore() {
    let root = TempDir::new();
    let r = root.path();
    common::git(r, &["init", "-q", "-b", "main"]);
    common::git(r, &["config", "user.email", "t@example.com"]);
    common::git(r, &["config", "user.name", "t"]);
    fs::write(r.join(".gitignore"), "ignored/\n").unwrap();
    fs::write(r.join("tracked.rs"), "v1").unwrap();
    common::git(r, &["add", "."]);
    common::git(r, &["commit", "-q", "-m", "init"]);
    sleep(Duration::from_millis(30));
    let since = SystemTime::now();
    sleep(Duration::from_millis(30));
    fs::write(r.join("tracked.rs"), "v2").unwrap();
    fs::create_dir_all(r.join("src/a/b/c/d")).unwrap();
    fs::write(r.join("src/a/b/c/d/deep_new.rs"), "x").unwrap(); // depth is unbounded in a repo
    fs::create_dir_all(r.join("ignored")).unwrap();
    fs::write(r.join("ignored/gen.rs"), "x").unwrap(); // .gitignore applies

    // Scan from a subdirectory: the repository top level is the root either way.
    let found = rel(r, &scan_changed_since(&r.join("src"), since));
    assert_eq!(
        found,
        [
            ("src/a/b/c/d/deep_new.rs".to_string(), true),
            ("tracked.rs".to_string(), false),
        ]
    );
}

#[test]
fn stamps_are_consumed_once_and_fall_back_without_a_tool_use_id() {
    let dir = TempDir::new();
    let at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    write_stamp(dir.path(), "sid", Some("toolu_01"), at);
    assert_eq!(take_stamp(dir.path(), "sid", Some("toolu_01")), Some(at));
    assert_eq!(
        take_stamp(dir.path(), "sid", Some("toolu_01")),
        None,
        "a stamp is read once"
    );
    // Without a tool-use id both sides use the session-level stamp.
    write_stamp(dir.path(), "sid", None, at);
    assert_eq!(take_stamp(dir.path(), "sid", None), Some(at));
    assert_eq!(take_stamp(dir.path(), "sid", Some("other")), None);
}
