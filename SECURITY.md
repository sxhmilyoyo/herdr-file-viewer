# Security

`herdr-file-viewer` is a **read-only** viewer that routinely opens **untrusted** content. It may
browse an agent's worktree, a fresh clone, or files from a collaborator. Its security controls
assume that this content is hostile.

## Threat model & mitigations

- **Read-only by construction.** The viewer never writes a file or mutates the git repository.
  Every `git` call uses read-only subcommands; opening a file in an editor is a hand-off to an
  external process, not an in-app edit.

- **The viewer neutralizes terminal controls in untrusted file content.** External renderers receive
  content on **stdin**, never as a command argument, so a file name cannot inject an argument.
  Before display, the viewer strips cursor movement, screen controls, OSC, C1, and other control
  sequences. It keeps only SGR color and style information, which it maps to ratatui styles. A
  malicious file can paint text only inside the viewer's region. It cannot move the cursor, clear
  the screen, set the window title, or control the terminal.

- **Remote notices use isolated, bounded display-only data.** They use fixed official HTTPS sources
  off the UI thread under one 15-second deadline. Private Git discovery excludes viewed-repo
  configuration but inherits global/system proxy and CA configuration; curl inherits ambient proxy
  settings and installed curl CA/TLS behavior. Curl starts with `.curlrc` disabled, uses a fixed
  authority over HTTPS only, and follows no redirects. A 1 MiB document cap is enforced in memory
  and on disk: curl's `--max-filesize` cannot bound a length-less (chunked) response, so the
  transient body file's size is watched during the transfer and an over-cap transfer is killed
  mid-stream. Accepted display content is bounded further still (spotlight title and body,
  combined release-details text); `404` withdraws a spotlight, while every other failure
  becomes typed, fail-silent outcomes. Remote Markdown reaches the configured renderer only on
  stdin and passes the terminal-control neutralizer, with no content-triggered actions. The
  complete, atomic, safe-to-delete cache (`update-check.json`) and explicit spotlight dismissal
  record (`spotlight-dismissal.json`) are the only viewer-owned persistent writes. The bounded
  dismissal record stores the last dismissed accepted title/body separately, so remote refreshes
  cannot overwrite it. Neither file affects the viewed root or Git repository.

- **Git invocations are hardened in untrusted repositories.** Because the opened repo may be
  hostile, queries disable configured clean, smudge, and process filters, and use `--no-ext-diff` /
  `--no-textconv` to refuse diff/textconv programs. Git 2.40+ also uses `--attr-source` to read
  worktree attributes from the empty tree. Concurrent hostile changes to Git configuration between
  filter inspection and query execution are outside this protection. `core.fsmonitor` and
  `core.hooksPath` are neutralized, `GIT_OPTIONAL_LOCKS=0` prevents index writes, and
  repo-redirecting environment variables (`GIT_DIR`, `GIT_WORK_TREE`, …) are scrubbed. This
  hardening lives in a single shared builder so it cannot drift between callers.

- **Untrusted session transcripts → defensive, fail-soft parsing.** The session view reads Claude
  Code's per-project transcript files (`~/.claude/projects/…`), whose content is agent- and
  user-derived and whose format is an undocumented internal (ADR-0011). Every line is parsed
  defensively: malformed JSON and unknown entry shapes are skipped, extracted paths are lexically
  normalized (relative or non-normalizable paths dropped), reads are read-only and incremental,
  and a missing or malformed transcript degrades to an empty view with a notice — never a crash.
  Displayed strings from a transcript (session titles, file names) pass the same control-byte
  sanitization every filesystem-derived string gets, and previewed member content flows through
  the standard untrusted-content pipeline above.

- **The session hook and its activity log → opt-in, bounded, state-dir only.** The session hook
  runs under Claude Code, once per file tool call, shell command, and prompt, and writes only to
  the plugin's own state directory: one append-only log per session plus short-lived stamp files.
  It never writes into a repository. Its input is Claude Code's hook payload, treated as
  untrusted: the session id must be a UUID-shaped token before it becomes a file name, tool paths
  are recorded as strings and lexically normalized by the viewer's reader (relative or
  non-normalizable paths dropped there), and any malformed payload is a silent no-op — the hook
  exits 0 without output in every case, so it can never block or fail a tool call. The disk scan
  it runs around a shell command is read-only (an `.gitignore`-aware walk, pruned of build and
  dependency trees, capped at 200 paths), and the one `git status` that classifies new files uses
  the same hardened builder as every other git call. `hook install` / `hook uninstall` are the
  only operations that modify a file outside the state directory — the user-scope Claude Code
  settings — and they run only when invoked explicitly, add or remove exactly the plugin's own
  entries, and write atomically.

- **Injection guards.** Host-supplied pane ids are validated before they reach an argv (so a
  flag-like id can't option-inject the herdr CLI); that includes the focused pane id the session
  view hands to `herdr agent get`, and the session id herdr answers with must be a UUID-shaped
  token before it is joined into a transcript or log path. Paths are passed to `git` as raw
  `OsStr` arguments after a within-root check (no traversal above the root, no arbitrary reads).

- **Resource bounds.** File reads and captured renderer/diff output are size-capped, and external
  renderers run under a wall-clock timeout, so a huge or slow input degrades gracefully rather
  than hanging or exhausting memory. Project content search skips files over 1 MiB and keeps at
  most 500 matching lines.

- **Crash containment.** The viewer catches renderer failures, including a panic on the render
  worker, and shows a non-fatal notice or placeholder instead of crashing.

## Reporting a vulnerability

Report suspected vulnerabilities through a **GitHub private security advisory**. Open this
repository's Security page and select "Report a vulnerability." Do not open a public issue.

We will acknowledge the report and provide a fix or mitigation plan after triage.
