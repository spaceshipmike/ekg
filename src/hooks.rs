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
/// group* (`kill -KILL -$$`, negative pgid) — which, because everything in
/// the tree inherited the same pgid, takes down the user command and any
/// children/grandchildren it spawned along with the wrapper and the
/// watchdog itself. That kill happens from a process that's part of the
/// hook's own tree, independent of ekg's tokio runtime or even ekg still
/// existing, so it fires whether or not ekg is still around to see it.
///
/// **No `--` before the negative pgid.** `kill -KILL -- -$$` (with the
/// POSIX "end of options" marker, the obvious/conventional way to write
/// this) is what an earlier revision shipped, and it's flatly broken under
/// `dash` — confirmed directly, not guessed: `dash`'s builtin `kill` fails
/// that exact invocation with `Illegal number: -` and never signals
/// anything, while the *identical* script minus `--` (`kill -KILL -$$`)
/// signals the group correctly. `dash` is `/bin/sh` on Debian/Ubuntu (this
/// wrapper's whole reason to exist is running as portable POSIX `sh`, and
/// `sh` on most Linux distros *is* `dash`), so this wasn't a hypothetical —
/// it silently broke the entire timeout/group-kill guarantee on Linux while
/// working fine in local dev on macOS (`/bin/sh` there is `bash`, which
/// parses the `--` form without complaint). `bash` accepts *both* forms
/// identically, so dropping `--` is not a trade-off between the two shells
/// — it is simply the form that works everywhere. There's no real ambiguity
/// risk from omitting it here either: once `-KILL`/`-TERM` has already been
/// consumed as the signal, the remaining `-$$` is the only positional
/// argument left, which is exactly the conventional `kill -SIGNAL -pgid`
/// idiom every `kill` implementation (dash's builtin, bash's builtin, and
/// the external `/bin/kill`) supports as its primary process-group form.
///
/// The *normal completion* path (the user command finishes before the
/// timeout) needs its own cleanup, and an earlier revision of this wrapper
/// got it wrong: it cancelled the watchdog and exited immediately, which
/// left two kinds of stragglers alive in the group after the wrapper
/// itself was gone —
///
/// - a user command that backgrounds its own child and doesn't wait on it
///   (`sleep 60 &` finishes the `-c` invocation instantly, `u` exits right
///   away, but the backgrounded `sleep 60` is still running);
/// - the watchdog subshell's own `sleep "$3"` — `kill "$w"` stops the
///   *subshell* but not the external `sleep` process it already forked as
///   its own child, which then just lives out its full timeout duration as
///   an orphan even though the hook itself is long done.
///
/// Fixed by having the wrapper sweep its *entire* group on the way out,
/// not just cancel the watchdog: after capturing the user command's exit
/// status, it runs `trap '' TERM` (makes itself immune to `SIGTERM` — it's
/// about to send that signal to its own group next) and then `kill -TERM
/// -$$` — a first, gentler pass, giving any straggler that traps
/// `SIGTERM` for its own cleanup a chance to use it.
///
/// `SIGTERM` alone isn't a complete guarantee, though: a straggler can
/// just as easily *ignore* `SIGTERM` (a hook backgrounds something like
/// `sh -c 'trap "" TERM; sleep 300'`), and would then survive
/// indefinitely — a revision that stopped at the TERM sweep had exactly
/// this gap. So after a short grace period (1s) for the TERM to take
/// effect on anything that's going to honor it, the wrapper follows up
/// with `kill -KILL -$$`. `SIGKILL` can't be trapped or ignored by
/// anything, so this second pass is unconditional: whatever's left in the
/// group, however stubborn, dies. The 1s grace is a deliberate trade —
/// one extra second of single-flight occupancy per *completed* hook, in
/// exchange for giving well-behaved stragglers (ones that trap `SIGTERM`
/// to flush state, close a connection, etc.) a real chance at a clean
/// shutdown instead of an unconditional `SIGKILL` every single time.
///
/// One consequence worth calling out: that final `kill -KILL -$$` also
/// kills the wrapper *itself* (same group, and unlike `SIGTERM` this one
/// isn't trapped) before it can reach `exit "$s"` — so on this path the
/// wrapper's own reported exit status becomes "killed by SIGKILL" rather
/// than the user command's real exit code. That's fine: ekg only ever
/// does a bare `child.wait()` on the wrapper (see
/// `spawn_hook_with_timeout`) and never inspects its exit status for
/// anything, so nothing downstream depends on `$s` surviving that final
/// kill.
///
/// The timeout path needs none of this: `SIGKILL` from the watchdog kills
/// the whole group (wrapper included) instantly, so none of this trailing
/// cleanup code ever runs there — it isn't needed, the group kill already
/// got everything in one unconditional pass.
///
/// Either way — normal completion or the watchdog's own timeout — ekg's
/// side only needs a plain `child.wait()` on the wrapper process; the
/// tree bounds, cleans up after itself (however stubborn its
/// descendants), and terminates on its own.
///
/// Positional args, passed to `sh -c` as separate argv entries — never
/// interpolated into this script's text — so arbitrary bytes in the user's
/// command (quotes, `$`, backticks, newlines, anything) can't break out of
/// `"$2"` and corrupt the wrapper's own syntax:
///   - `$0` = `"sh"` (conventional placeholder, unused)
///   - `$1` = the shell that runs the user's command (`$SHELL`, or `sh`)
///   - `$2` = the user's command string, passed verbatim to `"$1" -c`
///   - `$3` = timeout in seconds, as text (fractional allowed, e.g. `"0.05"`
///     — used by tests; production always passes [`HOOK_TIMEOUT`], which
///     formats as a plain integer, see `hook_timeout_formats_as_plain_integer_seconds`)
const HOOK_WRAPPER: &str = r#""$1" -c "$2" & u=$!; ( sleep "$3"; kill -KILL -$$ ) 2>/dev/null & w=$!; wait "$u"; s=$?; kill "$w" 2>/dev/null; trap '' TERM; kill -TERM -$$ 2>/dev/null; sleep 1; kill -KILL -$$ 2>/dev/null; exit "$s""#;

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

/// Every `EKG_*` var this module ever sets on a hook's environment.
/// Explicitly removed from the child's environment (see
/// `spawn_hook_with_timeout`) before applying whichever of them are
/// actually relevant to *this* invocation — otherwise a hook simply
/// inherits ekg's own process environment as-is, and a stray reserved var
/// already present there (most plausibly `EKG_OUTAGE_SECS=999 ekg
/// --on-outage ...`, but any of these) would leak through unchanged into
/// an invocation that never meant to set it. `--on-outage` in particular
/// only ever passes `EKG_HOST`/`EKG_OUTAGE_START` (see `outage_env`) —
/// `EKG_OUTAGE_SECS` is documented as recovery-only, so a hook script
/// checking for its presence to decide "is this an outage or a recovery"
/// must never see a leftover value from somewhere else.
const RESERVED_ENV_VARS: [&str; 3] = ["EKG_HOST", "EKG_OUTAGE_START", "EKG_OUTAGE_SECS"];

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
    // The wrapper pid is only useful to tests inspecting the process group
    // from outside; production has no need for it.
    let _ = spawn_hook_with_timeout(cmd, env, running, HOOK_TIMEOUT);
}

/// Spawns [`HOOK_WRAPPER`] (which in turn runs `cmd` via `$SHELL -c`,
/// falling back to `sh -c` if `$SHELL` is unset), with `env` applied on top
/// of the inherited environment (after first scrubbing any of
/// [`RESERVED_ENV_VARS`] that ekg's own environment happened to have set —
/// see that constant's doc comment), detached from the ping loop in its own
/// process group. Must be called from within a Tokio runtime (it is, from
/// `apply_event`, itself always called from the async main loop).
///
/// Spawn failures (bad `$SHELL`, command not found, etc.) are silently
/// dropped rather than surfaced — a broken hook must never crash ekg or
/// print anything that would corrupt the panel, mirroring "never block".
///
/// Returns the wrapper process's pid if it spawned successfully — unused by
/// production (`fire` doesn't care), but useful to tests that need to
/// inspect the hook's process group from outside (that pid *is* the
/// group's pgid, since the group leader's pgid always equals its own pid).
fn spawn_hook_with_timeout(
    cmd: &str,
    env: &[(String, String)],
    running: Arc<AtomicBool>,
    timeout: Duration,
) -> Option<u32> {
    let user_shell = resolve_shell(std::env::var("SHELL").ok());
    // The wrapper is always run via plain `sh`, not the user's `$SHELL` —
    // its own syntax (backgrounding, `wait pid`, `kill -$$`) is
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
    // Scrub every reserved var first, then apply this invocation's actual
    // set — order matters: if `env` ever legitimately included one of
    // these (it always does; `env` is `outage_env`/`recovery_env`'s
    // output), the explicit `.env()` below must win over the blanket
    // removal, not the other way around.
    for var in RESERVED_ENV_VARS {
        command.env_remove(var);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    // Own process group: a SIGINT from Ctrl-C (or a hangup) targets ekg's
    // foreground process group, which the wrapper (and everything it
    // spawns) would otherwise inherit and die alongside — silently
    // truncating a slow notification or a power-cycle command exactly when
    // the user disconnects and most wants it to keep running.
    // `process_group(0)` makes the wrapper its own group leader instead,
    // which doubles as the mechanism HOOK_WRAPPER's internal timeout (and
    // completion-path cleanup) use to reach its whole tree (`kill -$$`
    // targets this same group). Unix-only API; ekg only ships for
    // macOS/Linux (see README/CI), so no other-platform fallback is
    // needed. tokio::process::Command defines `process_group` natively
    // (mirroring std::os::unix::process::CommandExt) rather than requiring
    // that trait in scope.
    #[cfg(unix)]
    command.process_group(0);

    let Ok(mut child) = command.spawn() else {
        running.store(false, Ordering::SeqCst);
        return None;
    };
    let wrapper_pid = child.id();

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

    wrapper_pid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Poll budget for every event-based wait below. Deliberately generous
    /// (CI-scale, not laptop-scale): a shared/loaded CI runner can take
    /// seconds just to schedule a `fork`+`exec` of `sh`, and these tests
    /// previously used budgets in the 3-5s range that were comfortable
    /// locally but flaked under real CI contention (observed on both
    /// macOS and Ubuntu runners — see the commit that added this
    /// constant). On a healthy machine every one of these tests still
    /// finishes in a second or two, since `poll_until` returns as soon as
    /// the condition is true; this budget only matters as an upper bound
    /// for how long a slow environment gets before the test fails for a
    /// real reason.
    const TEST_POLL_BUDGET: Duration = Duration::from_secs(30);

    /// Internal `HOOK_WRAPPER` watchdog timeout used by the two tests that
    /// need the timeout to actually fire (as opposed to `HOOK_TIMEOUT`,
    /// production's real 30s value, used everywhere the timeout must *not*
    /// fire during the test). Deliberately more than the 1s these tests
    /// used to hardcode: `spawn_hook_with_timeout` always runs the test
    /// command via the *live* `$SHELL -c`, and a non-interactive shell
    /// startup is not guaranteed to be fast — a `.zshenv`/`.bashrc` doing
    /// real work (oh-my-zsh, direnv, nvm/rbenv hooks, secrets bootstrapping
    /// via a credential manager CLI) can easily take 1-3s even
    /// non-interactively, independent of anything ekg does. A 1s internal
    /// timeout races that startup cost and can fire before the shell even
    /// reaches the test's command, which one worker machine's
    /// multi-second, four-`op-read` `.zshenv` demonstrated concretely: the
    /// group-kill fired mid-startup, so the grandchild's pid was never
    /// written and the test failed for an environment reason that looks
    /// identical to a real group-kill regression. 5s keeps both tests fast
    /// relative to `TEST_POLL_BUDGET` while giving realistic shell startup
    /// costs comfortable headroom; it does not change what's being
    /// verified (the kill firing at all), only when it's allowed to fire.
    const GROUP_KILL_TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Polls `cond` until it's true or `budget` elapses, returning whether
    /// it succeeded. Used for every timing-sensitive assertion below in
    /// place of "sleep a fixed amount, then assert" — this whole suite
    /// spawns shell subprocesses, and a fixed sleep that's comfortably long
    /// on a quiet machine can be too short under CI contention, making the
    /// test flaky rather than wrong. Polling with a generous budget waits
    /// only as long as actually needed on a fast run while still tolerating
    /// a slow/loaded one; every assertion below is event-based ("did this
    /// eventually happen") rather than "after N ms this should be true".
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
    /// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) for two reasons:
    /// its guard is held across `.await` points below, which a std-mutex
    /// guard held across an await would trip clippy's `await_holding_lock`
    /// lint on (and is a real footgun on a multi-threaded runtime); and —
    /// relevant to test isolation specifically — `tokio::sync::Mutex` has
    /// no poisoning concept at all, so a test that panics while holding
    /// `_guard` (e.g. a failed `assert!`) simply drops and unlocks it
    /// normally, letting the next test acquire it cleanly. A
    /// `std::sync::Mutex` would instead poison on that panic and every
    /// subsequent `.lock()` would need `unwrap_or_else(|e| e.into_inner())`
    /// to avoid cascading every later test into a spurious poisoned-lock
    /// panic unrelated to what it's actually testing.
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

    /// Confirms `HOOK_TIMEOUT` (production's actual value, 30s) formats to
    /// a plain integer string with no decimal point or scientific
    /// notation. This is what actually gets passed as the wrapper's `$3`
    /// argv entry in production — pinned separately from (and faster/more
    /// reliable than) any test that has to spawn a shell to indirectly
    /// prove the same thing, since a wrong format here would make every
    /// production hook fail the same way regardless of how the tests
    /// happen to phrase their own timeouts.
    #[test]
    fn hook_timeout_formats_as_plain_integer_seconds() {
        assert_eq!(HOOK_TIMEOUT.as_secs_f64().to_string(), "30");
    }

    // The tests below spawn real subprocesses and assert on process-level
    // behavior (kill, wait, process groups) — `#[cfg(unix)]` since that
    // behavior (and the `sh`/`kill` syntax these tests and HOOK_WRAPPER use)
    // is POSIX-specific, matching ekg's own macOS/Linux-only support.

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_does_not_panic_on_ordinary_command() {
        let _guard = process_test_lock().lock().await;
        let running = Arc::new(AtomicBool::new(true));
        // Uses HOOK_TIMEOUT (production's real 30s value, not a
        // test-shortened one) specifically so this test also stands as
        // confirmation that the exact string production passes for `$3`
        // (see hook_timeout_formats_as_plain_integer_seconds) is one the
        // wrapper's `sleep "$3"` can actually parse and run with — a
        // hypothetical shell/`sleep` that choked on it would just hang
        // here until poll_until's budget expires and fails the test.
        spawn_hook_with_timeout("exit 0", &[], Arc::clone(&running), HOOK_TIMEOUT);
        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(
            cleared,
            "an ordinary command never cleared the running flag"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_survives_unresolvable_shell() {
        let _guard = process_test_lock().lock().await;
        std::env::set_var("SHELL", "/definitely/not/a/real/shell/binary");
        let running = Arc::new(AtomicBool::new(true));
        spawn_hook_with_timeout("echo hi", &[], Arc::clone(&running), HOOK_TIMEOUT);
        std::env::remove_var("SHELL");
        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(
            cleared,
            "an unresolvable $SHELL should still fail cleanly and clear the running flag"
        );
    }

    /// Pins the actual kill-on-timeout behavior: a command that would
    /// otherwise run far longer than any reasonable test budget is
    /// force-killed by a much shorter timeout, and the single-flight flag
    /// is cleared as a result — proof the kill (not a natural exit) cleared
    /// it. Both durations are whole seconds — no fractional `sleep`
    /// argument anywhere in this test — specifically to rule out any
    /// difference in how a given platform's `/bin/sh` (dash on Ubuntu,
    /// bash-as-sh on macOS, ...) or its external `sleep` binary parses a
    /// fractional-second argument; whole seconds are unambiguous
    /// everywhere. See the module's test-hardening commit for the CI
    /// flakiness this replaced.
    ///
    /// [`GROUP_KILL_TEST_TIMEOUT`]'s doc comment covers why this is 5s, not
    /// 1s.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_kills_and_reaps_after_timeout() {
        let _guard = process_test_lock().lock().await;
        let running = Arc::new(AtomicBool::new(true));
        spawn_hook_with_timeout(
            "sleep 300", // far longer than TEST_POLL_BUDGET could tolerate
            &[],
            Arc::clone(&running),
            GROUP_KILL_TEST_TIMEOUT,
        );
        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "timeout kill never cleared the running flag");
    }

    /// Checks whether a process is still alive via `kill -0`, the standard
    /// liveness probe — a zero exit means the pid exists (and is signalable
    /// by us), a nonzero exit means it doesn't. Uses `std::process::Command`
    /// synchronously (fine for a short-lived test-only check) rather than
    /// pulling in a `libc`/`nix` dependency just to call `kill(pid, 0)`
    /// directly.
    #[cfg(unix)]
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
    /// child. `sleep 300 &` backgrounds a grandchild (relative to
    /// `spawn_hook_with_timeout`'s own child, the wrapper) that a naive
    /// "kill just the wrapper's pid" implementation would leave running
    /// indefinitely past the timeout — 300s is far longer than this test
    /// (or its poll budgets) will ever wait, so the grandchild can only
    /// disappear via the group kill, never a coincidental natural exit.
    /// The grandchild's own pid is recorded to a file (via `$!`) so the
    /// test can check it directly, independent of the wrapper process's
    /// fate. `#[cfg(unix)]`: relies on POSIX process groups/signals and
    /// `sh` job-control syntax, matching ekg's own macOS/Linux-only
    /// support (see README/CI). [`GROUP_KILL_TEST_TIMEOUT`]'s doc comment
    /// covers why the internal timeout is 5s, not 1s.
    #[cfg(unix)]
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
        let cmd = format!("sleep 300 & echo $! > {}; wait", path.display());
        spawn_hook_with_timeout(&cmd, &[], Arc::clone(&running), GROUP_KILL_TEST_TIMEOUT);

        let recorded = poll_until(TEST_POLL_BUDGET, || {
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

        let dead = poll_until(TEST_POLL_BUDGET, || !pid_alive(grandchild_pid)).await;
        assert!(
            dead,
            "grandchild process (pid {grandchild_pid}) survived the timeout — group kill isn't reaching it"
        );
        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "wrapper's running flag was never cleared");

        let _ = std::fs::remove_file(&path);
    }

    /// Whether a `dash` binary is available on `PATH`. Not guaranteed on
    /// every dev machine (it happens to ship by default on macOS, and is
    /// `/bin/sh` on Debian/Ubuntu — installable via `brew install
    /// dash-shell` on macOS or already present on most Linux systems), so
    /// the dash-specific regression test below skips itself gracefully
    /// when it's absent rather than failing. The authoritative signal for
    /// a dash-specific regression is Ubuntu CI, which always has `dash` as
    /// `/bin/sh`; this test exists so the *same* class of bug is also
    /// catchable in local dev on any machine that happens to have `dash`
    /// installed, without requiring it.
    #[cfg(unix)]
    fn dash_available() -> bool {
        std::process::Command::new("dash")
            .arg("-c")
            .arg("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Regression test for the exact bug a round-4 Codex review caught on
    /// Ubuntu CI (and this test suite's own 30s budgets did NOT catch
    /// locally, because local dev here runs on macOS where `/bin/sh` is
    /// `bash`): `HOOK_WRAPPER`'s group-kill line used to read
    /// `kill -KILL -- -$$` — with the POSIX "end of options" `--` marker
    /// before the negative pgid, the obvious/conventional way to write it —
    /// and that is silently broken under `dash`'s **builtin** `kill`.
    /// Confirmed directly (not guessed) by running the exact line under
    /// `dash`: it fails with `dash: kill: Illegal number: -` and signals
    /// nothing, while the identical command minus `--` works correctly.
    /// `dash` is `/bin/sh` on Debian/Ubuntu, so every hook's timeout/
    /// group-kill guarantee was silently non-functional there.
    ///
    /// This spawns `HOOK_WRAPPER` through `dash` directly — bypassing
    /// `spawn_hook_with_timeout`'s hardcoded `Command::new("sh")`, which on
    /// this dev machine resolves to `bash` and would hide the bug — using
    /// the exact same repro Codex reported (`sleep 60 &`, a user command
    /// that backgrounds a child and returns immediately) to prove the fix
    /// (dropping `--`) actually works under the shell that broke it.
    #[cfg(unix)]
    #[tokio::test]
    async fn hook_wrapper_group_kill_works_under_dash() {
        if !dash_available() {
            eprintln!(
                "skipping hook_wrapper_group_kill_works_under_dash: no `dash` on PATH \
                 (install with `brew install dash-shell` on macOS to run this locally; \
                 Ubuntu CI always has it as /bin/sh)"
            );
            return;
        }
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-dash-repro-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Mirrors spawn_hook_with_timeout's own argv construction, except
        // the top-level interpreter is forced to `dash` regardless of what
        // `sh` resolves to on this machine.
        let cmd = format!("sleep 60 & echo $! > {}", path.display());
        let mut command = Command::new("dash");
        command
            .arg("-c")
            .arg(HOOK_WRAPPER)
            .arg("sh") // $0 — conventional, unused
            .arg("sh") // $1 — the user command's own shell; plain sh is enough here
            .arg(&cmd) // $2
            .arg("30") // $3 — long timeout; only the completion-path sweep should matter
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .expect("dash should be spawnable — dash_available() already checked");
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        let recorded = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert!(recorded, "backgrounded child's pid was never recorded");
        let bg_pid: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .expect("recorded pid should be a plain integer");

        let dead = poll_until(TEST_POLL_BUDGET, || !pid_alive(bg_pid)).await;
        assert!(
            dead,
            "backgrounded child (pid {bg_pid}) survived under dash — this is the exact bug \
             Ubuntu CI caught (`kill -- -$$` silently failing under dash's builtin kill)"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Lists the pids of every process currently in process group `pgid`,
    /// via `ps -A -o pid=,pgid=` (all processes, no headers). `-A` (not
    /// `-e`) specifically: `-e` means "all processes" on GNU `ps` but means
    /// something unrelated (append the environment) on BSD/macOS `ps` —
    /// `-A` is the one flag both agree on. Used by the completion-path
    /// cleanup tests below to confirm a hook's group is fully empty, not
    /// just that one specific pid is gone.
    #[cfg(unix)]
    fn process_group_members(pgid: u32) -> Vec<u32> {
        let Ok(output) = std::process::Command::new("ps")
            .arg("-A")
            .arg("-o")
            .arg("pid=,pgid=")
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let pid: u32 = fields.next()?.parse().ok()?;
                let pg: u32 = fields.next()?.parse().ok()?;
                (pg == pgid).then_some(pid)
            })
            .collect()
    }

    /// Pins the completion-path half of the group-sweep fix: Codex's exact
    /// repro was `cmd='sleep 60 &'` — the user command backgrounds a child
    /// and returns without waiting on it, so the wrapper's own `wait "$u"`
    /// returns almost immediately even though that grandchild is still
    /// running. Before HOOK_WRAPPER grew its trailing
    /// `trap '' TERM; kill -TERM -$$`, that grandchild would survive
    /// the hook's own completion entirely and only die (if ever) when the
    /// full timeout elapsed. A long (30s) timeout is used deliberately so
    /// the *timeout* path can't be what's killing it here — if the
    /// grandchild is dead well before that, it has to be the
    /// completion-path sweep.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_completion_kills_backgrounded_child_left_by_user_command() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-stray-bg-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let running = Arc::new(AtomicBool::new(true));
        let cmd = format!("sleep 60 & echo $! > {}", path.display());
        spawn_hook_with_timeout(&cmd, &[], Arc::clone(&running), Duration::from_secs(30));

        let recorded = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert!(recorded, "backgrounded child's pid was never recorded");
        let bg_pid: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .expect("recorded pid should be a plain integer");

        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(
            cleared,
            "the hook (which itself completes almost instantly) never cleared the running flag"
        );

        let dead = poll_until(TEST_POLL_BUDGET, || !pid_alive(bg_pid)).await;
        assert!(
            dead,
            "backgrounded child (pid {bg_pid}) survived the hook's own completion — the \
             completion-path group sweep isn't reaching it"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Pins the fix for the TERM-immunity gap a later review round caught:
    /// the completion-path sweep originally sent only `SIGTERM`, which a
    /// straggler can simply ignore (`sh -c 'trap "" TERM; sleep 300'`) and
    /// then survive indefinitely — the wrapper still gets reaped and the
    /// single-flight flag still clears, so a flapping link would
    /// accumulate one TERM-immune process per outage forever, exactly the
    /// "unbounded" outcome this whole module exists to prevent. `$!` after
    /// the backgrounded `sh -c '...'` captures *that* process's pid (the
    /// one that ignores TERM) as the thing to check — it doesn't matter
    /// that its own `sleep 300` child is nested one level deeper; both
    /// share the same process group and the group-wide kill reaches
    /// either one. A long (30s) hook timeout again rules out the timeout
    /// path as what's actually killing it — only the completion path's
    /// follow-up `SIGKILL` (after HOOK_WRAPPER's 1s TERM grace) can be
    /// responsible if it's dead well within that budget.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_completion_kills_term_immune_backgrounded_child() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-term-immune-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let running = Arc::new(AtomicBool::new(true));
        let cmd = format!(
            r#"sh -c 'trap "" TERM; sleep 300' & echo $! > {}"#,
            path.display()
        );
        spawn_hook_with_timeout(&cmd, &[], Arc::clone(&running), Duration::from_secs(30));

        let recorded = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert!(recorded, "TERM-immune child's pid was never recorded");
        let immune_pid: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .expect("recorded pid should be a plain integer");

        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "hook never cleared the running flag");

        let dead = poll_until(TEST_POLL_BUDGET, || !pid_alive(immune_pid)).await;
        assert!(
            dead,
            "TERM-immune process (pid {immune_pid}) survived the hook's own completion — \
             the completion-path sweep's SIGKILL follow-up isn't reaching it"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Same TERM-immunity repro as
    /// `spawn_hook_completion_kills_term_immune_backgrounded_child`, but
    /// through `dash` directly rather than whatever `sh` resolves to on
    /// this machine — see `hook_wrapper_group_kill_works_under_dash`'s doc
    /// comment for why that distinction matters (a bug in this exact area
    /// previously passed every local/macOS run while failing on Ubuntu).
    /// Confirms the SIGTERM-then-grace-then-SIGKILL sequence in
    /// HOOK_WRAPPER behaves the same under dash's builtin `trap`/`kill`/
    /// `wait` as it does under bash's.
    #[cfg(unix)]
    #[tokio::test]
    async fn hook_wrapper_kills_term_immune_child_under_dash() {
        if !dash_available() {
            eprintln!(
                "skipping hook_wrapper_kills_term_immune_child_under_dash: no `dash` on PATH"
            );
            return;
        }
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "ekg-hook-dash-term-immune-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let cmd = format!(
            r#"sh -c 'trap "" TERM; sleep 300' & echo $! > {}"#,
            path.display()
        );
        let mut command = Command::new("dash");
        command
            .arg("-c")
            .arg(HOOK_WRAPPER)
            .arg("sh")
            .arg("sh")
            .arg(&cmd)
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .expect("dash should be spawnable — dash_available() already checked");
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        let recorded = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert!(recorded, "TERM-immune child's pid was never recorded");
        let immune_pid: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .expect("recorded pid should be a plain integer");

        let dead = poll_until(TEST_POLL_BUDGET, || !pid_alive(immune_pid)).await;
        assert!(
            dead,
            "TERM-immune process (pid {immune_pid}) survived under dash — the SIGKILL \
             follow-up isn't reaching it there"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Pins the other half of the same fix: an *ordinary* quick hook (no
    /// stray backgrounding by the user command at all) must still leave
    /// zero processes behind in its own group — specifically including the
    /// watchdog subshell's own `sleep "$3"`, which `kill "$w"` alone
    /// doesn't reach (it kills the subshell, not the external `sleep`
    /// process that subshell already forked). Checks the group as a whole
    /// via `process_group_members` rather than guessing at individual
    /// pids, so this would also catch any other kind of straggler this
    /// wrapper might leave behind.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_completion_sweeps_entire_group_no_stragglers() {
        let _guard = process_test_lock().lock().await;
        let running = Arc::new(AtomicBool::new(true));
        let wrapper_pid =
            spawn_hook_with_timeout("exit 0", &[], Arc::clone(&running), Duration::from_secs(30))
                .expect("wrapper should have spawned");

        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "an ordinary quick hook never completed");

        let empty = poll_until(TEST_POLL_BUDGET, || {
            process_group_members(wrapper_pid).is_empty()
        })
        .await;
        assert!(
            empty,
            "hook's process group still has members after completion: {:?}",
            process_group_members(wrapper_pid)
        );
    }

    /// Pins the env-scrubbing fix: `EKG_OUTAGE_SECS` set in ekg's own
    /// process environment (Codex's example: `EKG_OUTAGE_SECS=999 ekg
    /// --on-outage ...`) must not leak into an `--on-outage` invocation,
    /// which never sets it itself (see `outage_env`). Uses
    /// `${EKG_OUTAGE_SECS+set}` — POSIX parameter expansion that's empty
    /// only when the variable is genuinely *unset*, not merely empty —
    /// so this distinguishes "scrubbed" from "present but blank", which a
    /// plain `-z` check on the value couldn't.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_hook_scrubs_stale_reserved_env_vars_from_parent() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-env-scrub-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Simulate a stray EKG_OUTAGE_SECS already present in ekg's own
        // environment — unrelated to this specific outage invocation.
        std::env::set_var("EKG_OUTAGE_SECS", "999");

        let running = Arc::new(AtomicBool::new(true));
        let cmd = format!(
            r#"printf '%s' "${{EKG_OUTAGE_SECS+set}}" > {}"#,
            path.display()
        );
        let env = outage_env("1.1.1.1", SystemTime::now());
        spawn_hook_with_timeout(&cmd, &env, Arc::clone(&running), Duration::from_secs(30));

        std::env::remove_var("EKG_OUTAGE_SECS");

        let cleared = poll_until(TEST_POLL_BUDGET, || !running.load(Ordering::SeqCst)).await;
        assert!(cleared, "hook never completed");

        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            contents, "",
            "EKG_OUTAGE_SECS leaked through from ekg's own environment into an \
             --on-outage hook, which never sets it itself"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Behavioral test of `Hooks`' single-flight gate: firing the same kind
    /// twice back-to-back while the first is still running (a slow command)
    /// must result in only one execution, not two stacked ones.
    #[cfg(unix)]
    #[tokio::test]
    async fn fire_outage_single_flight_skips_while_previous_still_running() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-singleflight-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let hooks = Hooks::new();
        // Whole-second sleep (not fractional) so this doesn't depend on how
        // a given platform's shell/`sleep` parses a fractional argument —
        // that's the actual *user command*, run by the resolved
        // `$SHELL`/`sh`, not the wrapper's own `$3`, but there's no reason
        // to depend on fractional support in either place when an integer
        // second works just as well for creating the overlap window this
        // test needs.
        let cmd = format!("sleep 1 && echo x >> {}", path.display());
        hooks.fire_outage(&cmd, &[]);
        hooks.fire_outage(&cmd, &[]); // should be skipped: first still running

        // Wait for the (single) expected write to land...
        assert!(
            poll_until(TEST_POLL_BUDGET, || line_count(&path) >= 1).await,
            "the one expected invocation never completed"
        );
        // ...then confirm a second write never arrives — poll for its
        // *absence* holding across a real wait window instead of a single
        // fixed sleep, so this is still event-based rather than "assume
        // nothing more happens after N ms". A skipped second invocation
        // never runs at all, so if none shows up by the time this budget
        // expires, that's the expected (negative) outcome, not a race.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            line_count(&path),
            1,
            "a second invocation ran even though the first was still in flight"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// A second event of the *same* kind after the first has finished must
    /// fire normally — single-flight only skips true overlap, it isn't a
    /// one-shot latch.
    ///
    /// The gate to wait on between the two `fire_outage` calls is the
    /// single-flight flag itself (`hooks.outage_running`, readable here
    /// since `tests` is a child module of `hooks` and the field isn't
    /// `pub` outside the crate but is visible within it) — not the file
    /// write. Those two things are no longer nearly-simultaneous now that
    /// HOOK_WRAPPER's completion path does a TERM sweep, a 1s grace, and a
    /// SIGKILL follow-up before it actually exits (see HOOK_WRAPPER's doc
    /// comment): the user command's visible effect (the `echo`) lands
    /// almost immediately, but the flag that actually gates a second
    /// `fire_outage` doesn't clear until the wrapper's full cleanup
    /// sequence finishes, roughly a second later. Firing again as soon as
    /// the file write is observed — before the flag has actually
    /// cleared — would just get silently skipped by single-flight, which
    /// is exactly what happened here until this test was corrected to
    /// wait on the real gate.
    #[cfg(unix)]
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
            poll_until(TEST_POLL_BUDGET, || line_count(&path) >= 1).await,
            "first invocation never completed"
        );
        assert!(
            poll_until(TEST_POLL_BUDGET, || {
                !hooks.outage_running.load(Ordering::SeqCst)
            })
            .await,
            "first invocation's single-flight flag never cleared"
        );
        hooks.fire_outage(&cmd, &[]);
        assert!(
            poll_until(TEST_POLL_BUDGET, || line_count(&path) >= 2).await,
            "second invocation, after the first completed, never fired"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `--on-outage` and `--on-recovery` are tracked independently: an
    /// in-flight outage hook must not block a recovery hook from firing.
    #[cfg(unix)]
    #[tokio::test]
    async fn outage_and_recovery_single_flight_are_independent() {
        let _guard = process_test_lock().lock().await;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-hook-independent-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let hooks = Hooks::new();
        let slow_cmd = format!("sleep 1 && echo outage >> {}", path.display());
        let fast_cmd = format!("echo recovery >> {}", path.display());
        hooks.fire_outage(&slow_cmd, &[]);
        hooks.fire_recovery(&fast_cmd, &[]);

        // The fast recovery hook should land well before the slow outage
        // hook's 1s sleep elapses.
        let recovered = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("recovery")
        })
        .await;
        assert!(recovered, "recovery hook never ran");
        assert!(!std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains("outage"));

        let outage_ran = poll_until(TEST_POLL_BUDGET, || {
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
    #[cfg(unix)]
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
        let slow_cmd = format!("sleep 1 && echo a >> {}", path.display());
        let fast_cmd = format!("echo b >> {}", path.display());

        host_a.fire_outage(&slow_cmd, &[]); // still "running" on host_a's Hooks
        host_b.fire_outage(&fast_cmd, &[]); // must not be skipped by host_a's flag

        // If single-flight state were (incorrectly) shared, host_b's fire
        // would have been dropped and "b" would never show up.
        let host_b_ran = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains('b')
        })
        .await;
        assert!(
            host_b_ran,
            "host_b's hook never ran — single-flight state leaked across targets"
        );

        let host_a_ran = poll_until(TEST_POLL_BUDGET, || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains('a')
        })
        .await;
        assert!(host_a_ran, "host_a's hook never ran");

        let _ = std::fs::remove_file(&path);
    }
}
