//! Outage command hooks (`--on-outage` / `--on-recovery`): run an arbitrary
//! user command, via the user's shell, when an outage is declared and when
//! it recovers — e.g. an ntfy/pushover push notification, an external log
//! line, or an automation like power-cycling a router.
//!
//! The command is spawned **detached** and never awaited inline: stdin,
//! stdout, and stderr are all discarded (`Stdio::null()`) so nothing the
//! command prints can corrupt the in-place terminal panel, and spawning it
//! must never block the ping loop even if the command is slow or hangs
//! (a flaky push-notification API, a router that takes a while to
//! power-cycle). We still don't want an unbounded pile of zombie processes
//! on a monitor that might run for days, so the child is reaped by a
//! detached `tokio::spawn` task that awaits it in the background — the ping
//! loop itself never waits on anything here.

use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

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

/// Spawns `cmd` via `$SHELL -c` (falling back to `sh -c` if `$SHELL` is
/// unset), with `env` applied on top of the inherited environment, detached
/// from the ping loop. Must be called from within a Tokio runtime (it is,
/// from `apply_event`, itself always called from the async main loop).
///
/// Spawn failures (bad `$SHELL`, command not found, etc.) are silently
/// dropped rather than surfaced — a broken hook must never crash ekg or
/// print anything that would corrupt the panel, mirroring "never block".
pub fn spawn_hook(cmd: &str, env: &[(String, String)]) {
    let sh = resolve_shell(std::env::var("SHELL").ok());
    let mut command = Command::new(&sh);
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in env {
        command.env(k, v);
    }

    if let Ok(mut child) = command.spawn() {
        // Reap the child on a detached background task instead of a bare
        // `Child` drop — dropping without waiting leaves a zombie process
        // until ekg itself exits, which would slowly accumulate over a
        // long-running (e.g. overnight, multi-day) monitoring session with
        // repeated outages. `wait()` here is async and runs off to the
        // side; the ping loop that called `spawn_hook` has already moved on.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_shell_uses_shell_env_when_set() {
        assert_eq!(resolve_shell(Some("/bin/zsh".to_string())), "/bin/zsh");
    }

    #[test]
    fn resolve_shell_falls_back_to_sh() {
        assert_eq!(resolve_shell(None), "sh");
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
        spawn_hook("exit 0", &[]);
        // Give the detached reap task a moment to run; nothing to assert
        // beyond "this didn't block or panic" — spawn_hook is fire-and-forget
        // by design.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn spawn_hook_survives_unresolvable_shell() {
        std::env::set_var("SHELL", "/definitely/not/a/real/shell/binary");
        spawn_hook("echo hi", &[]);
        std::env::remove_var("SHELL");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
