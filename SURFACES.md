# Surfaces

This project ships through multiple surfaces, and several depend on each other in
non-obvious ways. Before calling a change "done," sweep this list and ask which
surfaces it touches. Most changes touch one or two; the dangerous ones touch a
cross-cutting edge without looking like they do.

## The map

| Surface | Lives in | A change ripples here when you… | Verify |
|---|---|---|---|
| CLI flags & exit codes | `src/main.rs` (clap `Args`) | add/rename a flag, change a default, change exit semantics | `cargo test`; `--help` output reads correctly; README usage block matches |
| In-place terminal panel | `src/display.rs` | change stats, sparkline, colors, or any escape-sequence handling | run live in a normal and a narrow pane; Ctrl-C restores cursor + wrap |
| JSONL recorder | `src/recorder.rs` | touch the sample/outage record shapes or timestamps | `recorder::tests`; README "`--log` JSONL schema" section matches output |
| Outage hooks | `src/hooks.rs` | touch spawn/timeout/kill/env behavior or the shell wrapper | `hooks::tests` (incl. dash tests); 10 consecutive full-suite runs |
| Ping engine | `src/pinger.rs` | change interval, count bounding, socket, or supervisor behavior | `pinger::tests`; live run against a reachable + an unreachable host |
| README | `README.md` | change any user-visible behavior, flag, schema, or contract | re-read the touched section against the running binary |
| CI workflow | `.github/workflows/ci.yml` | change toolchain, test invocation, or platform matrix | both checks green; branch protection contexts still match job names |
| Release workflow | `.github/workflows/release.yml` | change the release/publish pipeline (cargo-dist output or the hand-hardening on top of it) | reapply tag-as-quoted-env-var + full-SHA action pinning after any regen; `dist-workspace.toml` `allow-dirty = ["ci"]` still present; actionlint clean |
| Homebrew tap | `spaceshipmike/homebrew-tap` (external repo) | a tagged release runs `publish-homebrew-formula` | formula in the tap repo matches the version/URL of the tag just released |

## Cross-cutting edges (the silent ones)

- `src/recorder.rs` timestamps ↔ `src/hooks.rs` `EKG_OUTAGE_START` — both are epoch **milliseconds** by deliberate agreement, so scripts parse one like the other. Changing either format breaks the pairing silently.
- `src/main.rs` outage-transition points (`apply_event`) fan out to **three** surfaces at once: the terminal outage line, the recorder's `outage_start`/`outage_end` records, and the hook invocations. A change to outage detection (threshold, dedup, cap) must be checked against all three — the `--max-outages` display cap deliberately does NOT apply to the recorder or hooks (they get the complete record).
- `src/display.rs` `Drop` ↔ every `std::process::exit` call site in `src/main.rs` — `Drop` never runs under `process::exit`, so each exit site needs its own `restore_cursor()`. Adding an exit path without one corrupts the terminal on that path only (invisible in tests).
- The hook shell wrapper (`HOOK_WRAPPER` in `src/hooks.rs`) ↔ Linux dash — the wrapper is interpreted by `/bin/sh`, which is bash on macOS but dash on Debian/Ubuntu. Any edit must keep the dash-exercising tests passing; bash-only syntax will pass everywhere except Linux.
- CI job names (`build + test (macos-latest)` / `build + test (ubuntu-latest)`) ↔ branch-protection required contexts — renaming a workflow job silently makes `main` unmergeable (or unprotected) until the protection rule is updated to match.
- README counted-dependency claim ("six dependencies") ↔ `Cargo.toml` `[dependencies]` — a new crate invalidates marketing copy that reviewers and users quote.
- `dist-workspace.toml` ↔ `.github/workflows/release.yml` — cargo-dist treats `release.yml` as generated output of `dist-workspace.toml`; `dist plan`/`dist host` fail their "workflow is up to date" check if the two drift without `allow-dirty = ["ci"]` set. Any edit to either file needs the other checked: a `dist-workspace.toml` config change should be followed by a regen (`dist generate`) with the hand-hardening reapplied; a hand edit to `release.yml` needs `allow-dirty` to stay set or it must exactly match fresh `dist generate` output.
- Git tag ↔ `Cargo.toml` `[package].version` — `release.yml` only fires on tags matching `**[0-9]+.[0-9]+.[0-9]+*`, and cargo-dist's `plan`/`host` steps reject a tag whose version doesn't match `Cargo.toml`. Bumping the version without an intent to tag-and-release is safe; tagging with a stale `Cargo.toml` version is not.
- Release pipeline output ↔ Homebrew tap formula in `spaceshipmike/homebrew-tap` — the pipeline does **not** publish to the tap (Homebrew publishing is disabled in `dist-workspace.toml`: the stock cargo-dist formula would omit completions/the man page, and no `HOMEBREW_TAP_TOKEN` secret exists). The tap formula is hand-maintained and builds from source; a version bump there must be done manually and can drift from the latest GitHub Release until someone updates it.

## The sweep

Before merging, ask:

1. Did this change what any flag, exit code, JSONL field, or `EKG_*` var means? → README + the contract entries in `INVARIANTS.md`.
2. Did this touch an outage transition? → check terminal line, recorder record, and hook firing together.
3. Did this add an exit path or shell snippet? → cursor restoration on that path; dash compatibility.
4. Did CI job names or the platform matrix change? → update branch-protection contexts.
