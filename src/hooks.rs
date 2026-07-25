//! Outage command hooks (`--on-outage` / `--on-recovery`): run an arbitrary
//! user command, via the user's shell, when an outage is declared and when
//! it recovers — e.g. an ntfy/pushover push notification, an external log
//! line, or an automation like power-cycling a router.
//!
//! Four properties the ping loop depends on, none of which a naive
//! `Command::spawn` + forget gives you for free:
//!
//! 1. **Never blocks the ping loop** — spawning is fire-and-forget; nothing
//!    here is ever awaited inline from `apply_event`.
//! 2. **Never corrupts the panel or gets killed by the terminal** — stdio is
//!    discarded, and the child runs in its own process group (`setpgid`) so
//!    a Ctrl-C or terminal hangup that targets ekg's foreground process
//!    group doesn't also kill a hook that's mid-flight (e.g. a slow
//!    power-cycle command actually needs to run to completion).
//! 3. **Never accumulates unbounded processes on a flapping link, even
//!    across the whole tree a hook command spawns, and even if ekg itself
//!    has already exited** — see [`HOOK_WRAPPER`]'s doc comment for how.
//! 4. **A flap storm on one target can't starve another's hooks** —
//!    single-flight (skip a new invocation while the previous one of the
//!    same kind is still running) is tracked per *target*, not globally;
//!    see `Hooks`'s doc comment.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

/// Hard ceiling on how long a hook command (and everything it spawns) may
/// run before the whole tree is killed. Generous enough for a real
/// notification API call or a router power-cycle script, but bounded so a
/// hung command can't accumulate forever — see the module doc's point 3.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// A POSIX `sh` script that runs the user's hook command with the whole
/// resulting process tree bounded by a timeout that's enforced **inside
/// that same tree**, not by ekg watching over it.
///
/// Earlier revisions had ekg's own tokio task `tokio::time::timeout` around
/// `child.wait()` and, on expiry, kill just that one child pid. Two bugs
/// followed from that design, both fixed by moving the timeout into the
/// spawned tree itself:
///
/// - Only the immediate child died. A hook command that's a pipeline or a
///   script spawning its own children (`cmd1 | cmd2`, a wrapper script that
///   forks a helper) left those descendants running past the 30s deadline —
///   and since the single-flight flag cleared once the immediate child was
///   reaped, a flapping link could still accumulate an unbounded number of
///   surviving grandchildren over time.
/// - The watchdog was a tokio task living inside ekg's own process. If ekg
///   exits (Ctrl-C, or immediately after the last `--count` event) while a
///   hook is still running, that task is dropped with it — a hung detached
///   hook then runs forever, exactly the guarantee `--on-outage`/
///   `--on-recovery` are documented as providing.
///
/// This script is spawned instead of the raw user command, as the process
/// group leader (`process_group(0)`, set by [`spawn_hook_with_timeout`]).
/// It backgrounds the user's command, backgrounds a watchdog subshell that
/// sleeps for the timeout and then sends `SIGKILL` to the *entire process
/// group* (`kill -KILL -- -$$`, negative pgid) — which, because everything
/// in the tree inherited the same pgid, takes down the user command and any
/// children/grandchildren it spawned along with the wrapper and the
/// watchdog itself. That kill happens from a process that's part of the
/// hook's own tree, independent of ekg's tokio runtime or even ekg still
/// existing, so it fires whether or not ekg is still around to see it.
/// Once the user command finishes on its own, the wrapper cancels the
/// watchdog and exits with the user command's exit status; ekg's side then
/// only needs a plain `child.wait()` on the wrapper process — the tree
/// bounds and terminates itself.
///
/// Positional args, passed to `sh -c` as separate argv entries — never
/// interpolated into this script's text — so arbitrary bytes in the user's
/// command (quotes, `$`, backticks, newlines, anything) can't break out of
/// `"$2"` and corrupt the wrapper's own syntax:
///   - `$0` = `"sh"` (conventional placeholder, unused)
///   - `$1` = the shell that runs the user's command (`$SHELL`, or `sh`)
///   - `$2` = the user's command string, passed verbatim to `"$1" -c`
///   - `$3` = timeout in seconds, as text (fractional allowed, e.g. `"0.05"`
///     — used by tests; production always passes [`HOOK_TIMEOUT`])
const HOOK_WRAPPER: &str = r#""$1" -c "$2" & u=$!; ( sleep "$3"; kill -KILL -- -$$ ) 2>/dev/null & w=$!; wait "$u"; s=$?; kill "$w" 2>/dev/null; exit "$s""#;

/// Owns the single-flight state for one target's `--on-outage` /
/// `--on-recovery`. `main` creates one `Hooks` per monitored target (it
/// lives inside that target's `TargetRuntime`) rather than one shared
/// across the whole session — `EKG_HOST` identifies which target changed
/// state, and a slow `--on-outage` for target A silently swallowing a
/// concurrent `--on-outage` for target B (a different host, a genuinely
/// separate event worth its own notification) would be a real loss, not a
/// convenience. Single-flight only within one target's own history of a
/// given hook kind — "don't stack repeated notifications for the *same*
/// flapping link" — is the actual goal.
pub struct Hooks {
    outage_running: Arc<AtomicBool>,
    recovery_running: Arc<AtomicBool>,
}

impl Hooks {
    pub fn new() -> Self {
        Self {
            outage_running: Arc::new(AtomicBool::new(false)),
            recovery_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Fires `--on-outage`. Single-flight: if a previous `--on-outage`
    /// invocation is still running, this call is silently skipped rather
    /// than stacked — see `should_fire`'s doc comment for why "skip" (not
    /// "kill the old one") is the right default for a notification hook.
    pub fn fire_outage(&self, cmd: &str, env: &[(String, String)]) {
        fire(cmd, env, Arc::clone(&self.outage_running));
    }

    /// Fires `--on-recovery`, with the same single-flight semantics as
    /// `fire_outage`, tracked independently (an in-flight `--on-outage`
    /// never blocks a `--on-recovery`, and vice versa).
    pub fn fire_recovery(&self, cmd: &str, env: &[(String, String)]) {
        fire(cmd, env, Arc::clone(&self.recovery_running));
    }
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new()
    }
}

/// Env vars passed to an `--on-outage` hook invocation. Split out from the
/// spawn call so the (worth unit-testing) construction logic doesn't need a
/// process or a tokio runtime to exercise.
pub fn outage_env(host: &str, started_wall: SystemTime) -> Vec<(String, String)> {
    vec![
        ("EKG_HOST".to_string(), host.to_string()),
        (
            "EKG_OUTAGE_START".to_string(),
            to_ms(started_wall).to_string(),
        ),
    ]
}

/// Env vars passed to an `--on-recovery` hook invocation: everything
/// `outage_env` provides, plus `EKG_OUTAGE_SECS` — the outage's whole-second
/// duration, since by recovery time it's known.
pub fn recovery_env(
    host: &str,
    started_wall: SystemTime,
    duration: Duration,
) -> Vec<(String, String)> {
    let mut env = outage_env(host, started_wall);
    env.push((
        "EKG_OUTAGE_SECS".to_string(),
        duration.as_secs().to_string(),
    ));
    env
}

/// `EKG_OUTAGE_START`'s format: milliseconds since the Unix epoch, matching
/// the `--log` recorder's `ts` field so scripts that already parse one can
/// parse the other the same way.
fn to_ms(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// Resolves which shell runs hook commands, given the current `$SHELL` env
/// var (or lack of one). Pulled out as a pure function of its input — rather
/// than reading `std::env::var` directly — so the fallback behavior is
/// unit-testable without mutating real process-global state.
fn resolve_shell(shell_var: Option<String>) -> String {
    shell_var.unwrap_or_else(|| "sh".to_string())
}

/// Decides whether a new hook invocation should proceed, given whether the
/// previous invocation of the *same kind* (`--on-outage` or `--on-recovery`,
/// tracked independently) was still running at the moment this one wanted
/// to start. Extracted as a pure predicate over that one bool — rather than
/// inlined into the atomic swap — so the single-flight decision itself is
/// unit-testable without spawning anything.
///
/// Skip (not "kill the old one and start fresh") is the chosen behavior:
/// these hooks exist for notifications and automations where the most
/// useful semantics on a flapping link are "at most one in flight, and
/// don't cut short whatever the first one was already doing" — a
/// power-cycle command in particular should be allowed to finish, not be
/// killed mid-cycle by a second flap arriving seconds later.
fn should_fire(previously_running: bool) -> bool {
    !previously_running
}

/// Single-flight gate + fire-and-forget spawn for one hook kind. If the
/// previous invocation of this kind hasn't finished, this call is a no-op;
/// otherwise it spawns via `spawn_hook_with_timeout` and the `running` flag
/// is cleared once that invocation completes (naturally, or by the timeout
/// kill) so the next event of this kind can fire.
fn fire(cmd: &str, env: &[(String, String)], running: Arc<AtomicBool>) {
    let previously_running = running.swap(true, Ordering::SeqCst);
    if !should_fire(previously_running) {
        return;
    }
    spawn_hook_with_timeout(cmd, env, running, HOOK_TIMEOUT);
}

/// Spawns [`HOOK_WRAPPER`] (which in turn runs `cmd` via `$SHELL -c`,
/// falling back to `sh -c` if `$SHELL` is unset), with `env` applied on top
/// of the inherited environment, detached from the ping loop in its own
/// process group. Must be called from within a Tokio runtime (it is, from
/// `apply_event`, itself always called from the async main loop).
///
/// Spawn failures (bad `$SHELL`, command not found, etc.) are silently
/// dropped rather than surfaced — a broken hook must never crash ekg or
/// print anything that would corrupt the panel, mirroring "never block".
fn spawn_hook_with_timeout(
    cmd: &str,
    env: &[(String, String)],
    running: Arc<AtomicBool>,
    timeout: Duration,
) {
    let user_shell = resolve_shell(std::env::var("SHELL").ok());
    // The wrapper is always run via plain `sh`, not the user's `$SHELL` —
    // its own syntax (backgrounding, `wait pid`, `kill -- -$$`) is
    // deliberately kept to portable POSIX `sh` so it behaves the same
    // regardless of what shell the user has configured; `$SHELL` is only
    // used *inside* the wrapper, to run the user's actual command as
    // before.
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(HOOK_WRAPPER)
        .arg("sh") // $0 — conventional, unused by the script
        .arg(&user_shell) // $1
        .arg(cmd) // $2
        .arg(timeout.as_secs_f64().to_string()) // $3
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        command.env(k, v);
    }
    // Own process group: a SIGINT from Ctrl-C (or a hangup) targets ekg's
    // foreground process group, which the wrapper (and everything it
    // spawns) would otherwise inherit and die alongside — silently
    // truncating a slow notification or a power-cycle command exactly when
    // the user disconnects and most wants it to keep running.
    // `process_group(0)` makes the wrapper its own group leader instead,
    // which doubles as the mechanism HOOK_WRAPPER's internal timeout uses
    // to kill its whole tree (`kill -- -$$` targets this same group).
    // Unix-only API; ekg only ships for macOS/Linux (see README/CI), so no
    // other-platform fallback is needed. tokio::process::Command defines
    // `process_group` natively (mirroring
    // std::os::unix::process::CommandExt) rather than requiring that trait
    // in scope.
    #[cfg(unix)]
    command.process_group(0);

    let Ok(mut child) = command.spawn() else {
        running.store(false, Ordering::SeqCst);
        return;
    };

    // Reap the wrapper on a detached background task instead of a bare
    // `Child` drop — dropping without waiting leaves a zombie process until
    // ekg itself exits, which would slowly accumulate over a long-running
    // (overnight, multi-day) monitoring session with repeated outages. No
    // timeout wrapper needed here: HOOK_WRAPPER's own internal watchdog
    // bounds and self-terminates the whole process tree (including any
    // children the user command spawns) independent of this task, so a
    // plain wait is enough — and correct even if ekg exits before this
    // task ever runs again, since the watchdog lives inside the hook's own
    // tree, not ekg's.
    tokio::spawn(async move {
        let _ = child.wait().await;
        running.store(false, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polls `cond` until it's true or `budget` elapses, returning whether
    /// it succeeded. Used instead of a single fixed `sleep` for the
    /// process-spawning tests below: this whole suite runs with many tests
    /// spawning shell subprocesses concurrently, and a fixed short sleep
    /// (e.g. 100-600ms) that's comfortably long on a quiet machine can be
    /// too short under that contention, making the test flaky rather than
    /// wrong. Polling with a generous budget (seconds, not tens of
    /// milliseconds) waits only as long as actually needed on a fast run
    /// while still tolerating a slow/loaded one.
    async fn poll_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = tokio::time::Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= budget {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn line_count(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Guards the tests below that actually spawn shell subprocesses and
    /// make timing assertions about them. `cargo test` runs test functions
    /// concurrently across OS threads by default; without this, several of
    /// these tests spawning `sleep`/`echo` subprocesses at once on a
    /// resource-constrained runner can stall each other past their poll
    /// budgets, flaking on process-scheduling contention that has nothing
    /// to do with the single-flight/timeout logic actually under test. This
    /// only serializes these tests *against each other* — the concurrency
    /// each test exercises internally (e.g. two `fire_outage` calls racing)
    /// is unaffected.
    ///
    /// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) specifically
    /// because its guard is held across `.await` points below — a
    /// std-mutex guard held across an await is a clippy
    /// `await_holding_lock` lint (and a real footgun on a multi-threaded
    /// runtime), whereas an async mutex is designed for exactly this.
    fn process_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[test]
    fn resolve_shell_uses_shell_env_when_set() {
        assert_eq!(resolve_shell(Some("/bin/zsh".to_string())), "/bin/zsh");
    }

    #[test]
    fn resolve_shell_falls_back_to_sh() {
        assert_eq!(resolve_shell(None), "sh");
    }

    #[test]
    fn should_fire_when_nothing_previously_running() {
        assert!(should_fire(false));
    }

    #[test]
    fn should_not_fire_when_previous_invocation_still_running() {
        assert!(!should_fire(true));
    }

    #[test]
    fn outage_env_has_host_and_start() {
        let start = UNIX_EPOCH + Duration::from_millis(1_690_300_003_123);
        let env = outage_env("1.1.1.1", start);
        assert_eq!(
            env,
            vec![
                ("EKG_HOST".to_string(), "1.1.1.1".to_string()),
                ("EKG_OUTAGE_START".to_string(), "1690300003123".to_string()),
            ]
        );
    }

    #[test]
    fn recovery_env_adds_whole_second_duration() {
        let start = UNIX_EPOCH + Duration::from_millis(1_690_300_003_123);
        let env = recovery_env("router.local", start, Duration::from_millis(36_500));
        assert_eq!(
            env,
            vec![
                ("EKG_HOST".to_string(), "router.local".to_string()),
                ("EKG_OUTAGE_START".to_string(), "1690300003123".to_string()),
                ("EKG_OUTAGE_SECS".to_string(), "36".to_string()),
            ]
        );
    }

    #[test]
    fn recovery_env_truncates_sub_second_duration_to_zero() {
        let start = SystemTime::now();
        let env = recovery_env("h", start, Duration::from_millis(400));
        assert_eq!(env.last().unwrap().1, "0");
    }

    #[tokio::test]
    async fn spawn_hook_does_not_panic_on_ordinary_command() {
        let _guard = process_test_lock().lock().await;
        let running = Arc::new(AtomicBool::new(true));
        spawn_hook_with_timeout("exit 0", &[], Arc::clone(&running), HOOK_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!running.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn spawn_hook_survives_unresolvable_shell() {
        let _guard = process_test_lock().lock().await;
        std::env::set_var("SHELL", "/definitely/not/a/real/shell/binary");
        let running = Arc::new(AtomicBool::new(true));
        spawn_hook_with_timeout("echo hi", &[], Arc::clone(&running), HOOK_TIMEOUT);
        std::env::remove_var("SHELL");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!running.load(Ordering::SeqCst));
    }

    /// Pins the actual kill-on-timeout behavior: a command that would
    /// otherwise run for 5s is force-killed by a much shorter timeout, and
    /// the single-flight flag is cleared well before the 5s would have
    /// elapsed naturally — proof the kill (not just the natural exit)
    /// cleared it.
    #[tokio::test]
    async fn spawn_hook_kills_and_reaps_after_timeout() {
        let _guard = process_test_lock().lock().await;
        let running = Arc::new(AtomicBool::new(true));
        spawn_hook_with_timeout(
            "sleep 5",
            &[],
            Arc::clone(&running),
            Duration::from_millis(50),
        );
        // The kill fires at 50ms; poll well past that but far short of the
        // uninterrupted 5s the command would otherwise take, so this test
        // still fails loudly if the kill stops working.
        let cleared = poll_until(Duration::from_secs(3), || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "timeout kill never cleared the running flag");
    }

    /// Checks whether a process is still alive via `kill -0`, the standard
    /// liveness probe — a zero exit means the pid exists (and is signalable
    /// by us), a nonzero exit means it doesn't. Uses `std::process::Command`
    /// synchronously (fine for a short-lived test-only check) rather than
    /// pulling in a `libc`/`nix` dependency just to call `kill(pid, 0)`
    /// directly.
    fn pid_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Pins the fix for the group-kill bug: the timeout must kill the
    /// *entire* process group the wrapper leads, not just its immediate
    /// child. `sleep 60 &` backgrounds a grandchild (relative to
    /// `spawn_hook_with_timeout`'s own child, the wrapper) that a naive
    /// "kill just the wrapper's pid" implementation would leave running
    /// indefinitely past the timeout. The grandchild's own pid is recorded
    /// to a file (via `$!`) so the test can check it directly, independent
    /// of the wrapper process's fate.
    #[tokio::test]
    async fn spawn_hook_kills_whole_group_including_grandchildren() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "ekg-hook-grandchild-pid-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let running = Arc::new(AtomicBool::new(true));
        let cmd = format!("sleep 60 & echo $! > {}; wait", path.display());
        spawn_hook_with_timeout(&cmd, &[], Arc::clone(&running), Duration::from_millis(100));

        let recorded = poll_until(Duration::from_secs(3), || {
            std::fs::read_to_string(&path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert!(recorded, "grandchild pid was never recorded");
        let grandchild_pid: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .expect("recorded pid should be a plain integer");
        assert!(
            pid_alive(grandchild_pid),
            "grandchild should still be alive right after it's recorded, before the timeout fires"
        );

        // The 100ms timeout fires well before the grandchild's own 60s
        // sleep would end naturally — if it's gone by the time this poll
        // succeeds (comfortably within budget), that's the group kill, not
        // a coincidental natural exit.
        let dead = poll_until(Duration::from_secs(3), || !pid_alive(grandchild_pid)).await;
        assert!(
            dead,
            "grandchild process (pid {grandchild_pid}) survived the timeout — group kill isn't reaching it"
        );
        assert!(!running.load(Ordering::SeqCst));

        let _ = std::fs::remove_file(&path);
    }

    /// Behavioral test of `Hooks`' single-flight gate: firing the same kind
    /// twice back-to-back while the first is still running (a slow command)
    /// must result in only one execution, not two stacked ones.
    #[tokio::test]
    async fn fire_outage_single_flight_skips_while_previous_still_running() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-singleflight-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let hooks = Hooks::new();
        // Deliberately slow relative to the poll budget below, so the
        // second `fire_outage` call unambiguously lands while the first is
        // still in flight rather than racing it.
        let cmd = format!("sleep 0.5 && echo x >> {}", path.display());
        hooks.fire_outage(&cmd, &[]);
        hooks.fire_outage(&cmd, &[]); // should be skipped: first still running

        // Wait for the (single) expected write to land...
        assert!(poll_until(Duration::from_secs(5), || line_count(&path) >= 1).await);
        // ...then confirm nothing further arrives after that.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(line_count(&path), 1);

        let _ = std::fs::remove_file(&path);
    }

    /// A second event of the *same* kind after the first has finished must
    /// fire normally — single-flight only skips true overlap, it isn't a
    /// one-shot latch.
    #[tokio::test]
    async fn fire_outage_fires_again_after_previous_completes() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-refire-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let hooks = Hooks::new();
        let cmd = format!("echo x >> {}", path.display());
        hooks.fire_outage(&cmd, &[]);
        assert!(
            poll_until(Duration::from_secs(5), || line_count(&path) >= 1).await,
            "first invocation never completed"
        );
        hooks.fire_outage(&cmd, &[]);
        assert!(
            poll_until(Duration::from_secs(5), || line_count(&path) >= 2).await,
            "second invocation, after the first completed, never fired"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `--on-outage` and `--on-recovery` are tracked independently: an
    /// in-flight outage hook must not block a recovery hook from firing.
    #[tokio::test]
    async fn outage_and_recovery_single_flight_are_independent() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-independent-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let hooks = Hooks::new();
        let slow_cmd = format!("sleep 0.5 && echo outage >> {}", path.display());
        let fast_cmd = format!("echo recovery >> {}", path.display());
        hooks.fire_outage(&slow_cmd, &[]);
        hooks.fire_recovery(&fast_cmd, &[]);

        // The fast recovery hook should land well before the slow outage
        // hook's 0.5s sleep elapses.
        let recovered = poll_until(Duration::from_secs(3), || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("recovery")
        })
        .await;
        assert!(recovered, "recovery hook never ran");
        assert!(!std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("outage"));

        let outage_ran = poll_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("outage")
        })
        .await;
        assert!(outage_ran, "outage hook never ran");

        let _ = std::fs::remove_file(&path);
    }

    /// Per-target independence (the fix for the third review round's
    /// single-flight bug): `main` now gives each target its own `Hooks`
    /// instance rather than sharing one across the whole session. This test
    /// pins that separate instances never suppress each other — a slow
    /// `--on-outage` "in flight" for one target's `Hooks` must not skip a
    /// concurrent `--on-outage` fired on a *different* target's `Hooks`,
    /// the way it would if the flag were shared/global.
    #[tokio::test]
    async fn separate_hooks_instances_do_not_share_single_flight_state() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-per-target-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Two independent `Hooks`, standing in for two `TargetRuntime`s'
        // worth of state (host A and host B).
        let host_a = Hooks::new();
        let host_b = Hooks::new();
        let slow_cmd = format!("sleep 0.4 && echo a >> {}", path.display());
        let fast_cmd = format!("echo b >> {}", path.display());

        host_a.fire_outage(&slow_cmd, &[]); // still "running" on host_a's Hooks
        host_b.fire_outage(&fast_cmd, &[]); // must not be skipped by host_a's flag

        // If single-flight state were (incorrectly) shared, host_b's fire
        // would have been dropped and "b" would never show up.
        let host_b_ran = poll_until(Duration::from_secs(3), || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains('b')
        })
        .await;
        assert!(
            host_b_ran,
            "host_b's hook never ran — single-flight state leaked across targets"
        );

        let host_a_ran = poll_until(Duration::from_secs(5), || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains('a')
        })
        .await;
        assert!(host_a_ran, "host_a's hook never ran");

        let _ = std::fs::remove_file(&path);
    }
}
