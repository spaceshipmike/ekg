//! Async ping loop wrapping `surge-ping`'s unprivileged (SOCK_DGRAM) ICMP
//! client. Emits one `PingEvent` per tick at the configured interval.
//!
//! Socket-kind selection is explicit (IPv4 vs IPv6, matched to the target's
//! address family) rather than hardcoded, and client creation happens
//! up front (in `create_client`, called from `main` before the loop starts)
//! so a failure to open the unprivileged ICMP socket surfaces as one clear,
//! actionable error message instead of a silent task death or a panic.
//! This matters most cross-platform: macOS ships unprivileged ICMP
//! (`SOCK_DGRAM`/`IPPROTO_ICMP`) enabled by default, but Linux gates it
//! behind `net.ipv4.ping_group_range` — a box where that sysctl excludes the
//! running user's group will fail here, and the error message says so.

use std::net::IpAddr;
use std::time::Duration;

use surge_ping::{Client, Config, PingIdentifier, PingSequence, ICMP};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::stats::Sample;

/// A single ping outcome, timestamped implicitly by arrival on the channel.
#[derive(Debug, Clone, Copy)]
pub enum PingEvent {
    Reply(Duration),
    Timeout,
}

/// A `PingEvent` tagged with the index of the target it came from, so
/// multiple per-target ping loops can multiplex onto one shared channel.
#[derive(Debug, Clone, Copy)]
pub struct TaggedEvent {
    pub idx: usize,
    pub event: PingEvent,
}

impl From<PingEvent> for Sample {
    fn from(e: PingEvent) -> Self {
        match e {
            PingEvent::Reply(d) => Sample::Reply(d),
            PingEvent::Timeout => Sample::Timeout,
        }
    }
}

/// Per-ping timeout: generous enough to distinguish "slow" from "dropped"
/// without stalling the tick loop under normal intervals.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Creates the unprivileged ICMP client for the target's address family
/// (v4 vs v6 selected explicitly, not assumed). On failure, returns a
/// human-actionable message rather than propagating the raw OS error alone.
pub fn create_client(target: IpAddr) -> Result<Client, String> {
    let kind = if target.is_ipv6() { ICMP::V6 } else { ICMP::V4 };
    let config = Config::builder().kind(kind).build();
    Client::new(&config).map_err(|e| {
        format!(
            "could not open an unprivileged ICMP socket ({e}).\n\
             \n\
             This usually means the OS is blocking unprivileged (SOCK_DGRAM) ICMP:\n\
             - Linux: check `sysctl net.ipv4.ping_group_range` — the running user's\n\
             group must fall inside that range (e.g. `sudo sysctl -w \\\n\
             net.ipv4.ping_group_range=\"0 2147483647\"` to allow all groups).\n\
             - macOS: unprivileged ICMP is normally enabled by default; if this\n\
             still fails, check for a restrictive sandbox/profile blocking raw/\n\
             datagram ICMP sockets for this process.\n\
             ekg does not fall back to a privileged (raw/sudo) socket."
        )
    })
}

/// Spawns the ping loop for one target on the current tokio runtime, plus a
/// small supervisor task that watches it. Events are tagged with `idx` (the
/// target's position in the CLI's target list) and sent over the shared
/// `tx`, so any number of targets can multiplex their independent ping
/// loops (each with its own client/socket) onto one channel that `main`
/// selects over.
///
/// The ping loop's only normal exit path is `tx.send(..).await` failing,
/// which only happens once `main`'s receiver has been dropped — i.e. after
/// `main`'s event loop has already ended. So while `main` is still running,
/// this task ending (whether it returns normally or panics) always means
/// something went wrong for this target, and a dead-but-undetected pinger
/// would otherwise leave that target frozen at its last rendered state
/// forever (the shared `mpsc::Receiver` only closes once *every* sender —
/// i.e. every target's task — has dropped its clone, so one dead target
/// does not end the others). The supervisor awaits the ping loop's
/// `JoinHandle` (which resolves on either a normal return or a caught
/// panic) and reports `idx` on `death_tx` so `main` can detect it, restore
/// the terminal, and exit instead of silently rendering a stale row
/// forever.
pub fn spawn(
    client: Client,
    host: IpAddr,
    interval: Duration,
    idx: usize,
    tx: mpsc::Sender<TaggedEvent>,
    death_tx: mpsc::Sender<usize>,
) {
    let handle = tokio::spawn(async move {
        let mut pinger = client
            .pinger(host, PingIdentifier(std::process::id() as u16))
            .await;
        pinger.timeout(PING_TIMEOUT);

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let payload = [0u8; 56];
        let mut seq: u16 = 0;

        loop {
            ticker.tick().await;
            let event = match pinger.ping(PingSequence(seq), &payload).await {
                Ok((_packet, rtt)) => PingEvent::Reply(rtt),
                Err(_) => PingEvent::Timeout,
            };
            seq = seq.wrapping_add(1);
            if tx.send(TaggedEvent { idx, event }).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        // `Err` here means the ping loop panicked (tokio catches the
        // unwind and reports it via `JoinError`); `Ok(())` means it
        // returned normally. Either way, while `main` is still alive that
        // is unexpected — see the doc comment above.
        let _ = handle.await;
        let _ = death_tx.send(idx).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the detection wiring itself: constructing a real dead pinger
    /// (e.g. a target address that fails mid-run) isn't practical to do
    /// cheaply in a test, but the supervisor pattern `spawn` uses — await
    /// the ping loop's `JoinHandle`, then report `idx` on `death_tx` — is
    /// exactly what this exercises directly, panic included.
    #[tokio::test]
    async fn supervisor_reports_idx_when_watched_task_panics() {
        let (death_tx, mut death_rx) = mpsc::channel::<usize>(1);
        let idx = 7usize;

        let handle = tokio::spawn(async { panic!("simulated pinger death") });
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = death_tx.send(idx).await;
        });

        let got = death_rx.recv().await;
        assert_eq!(got, Some(idx));
    }

    /// Same wiring, but for a clean early return rather than a panic — the
    /// ping loop's own normal exit path (`tx.send(..).await` failing) is
    /// exactly this shape once `main`'s receiver is gone.
    #[tokio::test]
    async fn supervisor_reports_idx_when_watched_task_returns_normally() {
        let (death_tx, mut death_rx) = mpsc::channel::<usize>(1);
        let idx = 3usize;

        let handle = tokio::spawn(async {});
        tokio::spawn(async move {
            let _ = handle.await;
            let _ = death_tx.send(idx).await;
        });

        let got = death_rx.recv().await;
        assert_eq!(got, Some(idx));
    }
}
