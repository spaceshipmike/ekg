mod display;
mod pinger;
mod recorder;
mod stats;

use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;

use display::{Display, MultiTargetRow, OUTAGE_THRESHOLD};
use pinger::{PingEvent, TaggedEvent};
use recorder::Recorder;
use stats::{Sample, Stats};

/// ekg — compact in-place ping monitor.
#[derive(Parser, Debug)]
#[command(name = "ekg", version, about = "Compact in-place ping monitor")]
struct Args {
    /// Target host(s) or IP(s) to ping. Give more than one to monitor
    /// several targets side by side (e.g. a router and the open internet,
    /// to see whose fault an outage is).
    #[arg(default_value = "1.1.1.1")]
    hosts: Vec<String>,

    /// Ping interval in seconds (fractions allowed, e.g. 0.5). Applies to
    /// every target.
    #[arg(short, long, default_value_t = 1.0)]
    interval: f64,

    /// Rolling window size (number of samples kept for stats/sparkline).
    /// Applies to every target.
    #[arg(short, long, default_value_t = 60)]
    window: usize,

    /// Stop printing permanent outage lines after this many per target (0
    /// disables them entirely). The panel's "last outage" line and the
    /// final summary still reflect every outage. With multiple targets the
    /// cap notice itself still prints at most once overall.
    #[arg(short, long)]
    max_outages: Option<u32>,

    /// Send this many pings per target, then print the summary and exit
    /// (instead of running until Ctrl-C). Exit code reflects whether loss
    /// stayed within --max-loss. `--count 0` sends nothing and exits
    /// immediately (0/0 loss counts as a pass).
    #[arg(short = 'c', long)]
    count: Option<u64>,

    /// With --count, the maximum lifetime loss percentage still considered
    /// a success (exit code 0). Default 0: any packet loss at all is a
    /// failing run. Ignored without --count. Must be a finite value in
    /// 0.0..=100.0 — anything outside that range (including inf/NaN) would
    /// make a --count health check silently exit 0 no matter how bad the
    /// loss was, so clap rejects it up front.
    #[arg(long, default_value_t = 0.0, value_parser = parse_max_loss)]
    max_loss: f64,

    /// Append each ping sample, plus outage start/end events, to this file
    /// as newline-delimited JSON. Opened in append mode and flushed after
    /// every line, so an overnight run's log survives an interruption.
    #[arg(long)]
    log: Option<PathBuf>,
}

/// Resolves the host argument to an IP address, accepting either a literal
/// IP or a hostname.
fn resolve_host(host: &str) -> std::io::Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    let mut addrs = (host, 0u16).to_socket_addrs()?;
    addrs
        .next()
        .map(|a| a.ip())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "could not resolve host"))
}

/// Per-target runtime state: rolling stats plus the outage/recovery state
/// machine that used to live as loose locals in `main` for a single target.
/// One of these exists per CLI-supplied host.
struct TargetRuntime {
    host: String,
    stats: Stats,
    consecutive_timeouts: u32,
    down_since: Option<Instant>,
    down_wall_start: Option<SystemTime>,
    last_recovery: Instant,
    outage_count: u32,
    /// Formatted "last outage: HH:MM:SS (Ns)" string, used verbatim on the
    /// single-target panel's line 3.
    last_outage_summary: Option<String>,
}

impl TargetRuntime {
    fn new(host: String, window: usize, session_start: Instant) -> Self {
        Self {
            host,
            stats: Stats::new(window),
            consecutive_timeouts: 0,
            down_since: None,
            down_wall_start: None,
            last_recovery: session_start,
            outage_count: 0,
            last_outage_summary: None,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    // --count 0: nothing to send, so there's nothing to wait on either.
    // Handled before any host resolution, client/socket creation, or
    // terminal setup so it's truly immediate and never touches the
    // network. Loss is 0/0 == 0.0%, which is a pass against any
    // non-negative --max-loss (the default 0.0 included).
    if args.count == Some(0) {
        let session_start = Instant::now();
        let targets: Vec<TargetRuntime> = args
            .hosts
            .iter()
            .map(|h| TargetRuntime::new(h.clone(), args.window, session_start))
            .collect();
        print_summary(&targets, session_start);
        let ok = targets
            .iter()
            .all(|t| loss_within_threshold(t.stats.lifetime_loss_pct(), args.max_loss));
        std::process::exit(if ok { 0 } else { 1 });
    }

    let mut resolved: Vec<(String, IpAddr)> = Vec::with_capacity(args.hosts.len());
    for host in &args.hosts {
        match resolve_host(host) {
            Ok(ip) => resolved.push((host.clone(), ip)),
            Err(e) => {
                eprintln!("ekg: could not resolve '{host}': {e}");
                std::process::exit(1);
            }
        }
    }

    let mut clients = Vec::with_capacity(resolved.len());
    for (host, ip) in &resolved {
        match pinger::create_client(*ip) {
            Ok(c) => clients.push(c),
            Err(msg) => {
                eprintln!("ekg: {host}: {msg}");
                std::process::exit(1);
            }
        }
    }

    let mut recorder: Option<Recorder> = match &args.log {
        Some(path) => match Recorder::open(path) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("ekg: could not open log file '{}': {e}", path.display());
                std::process::exit(1);
            }
        },
        None => None,
    };

    let interval = Duration::from_secs_f64(args.interval.max(0.001));
    let multi = resolved.len() > 1;

    let mut display = Display::new();
    display.hide_cursor()?;

    // Panic hook: always restore the cursor even if we panic mid-render.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = std::io::stdout();
        use std::io::Write;
        let _ = crossterm::execute!(
            out,
            crossterm::cursor::Show,
            crossterm::terminal::EnableLineWrap
        );
        let _ = out.flush();
        default_hook(info);
    }));

    let session_start = Instant::now();

    let mut targets: Vec<TargetRuntime> = resolved
        .iter()
        .map(|(host, _)| TargetRuntime::new(host.clone(), args.window, session_start))
        .collect();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<TaggedEvent>(32.max(8 * resolved.len()));
    let (death_tx, mut death_rx) = tokio::sync::mpsc::channel::<usize>(resolved.len().max(1));
    for (idx, ((_, ip), client)) in resolved.iter().zip(clients.into_iter()).enumerate() {
        pinger::spawn(
            client,
            *ip,
            interval,
            idx,
            args.count,
            tx.clone(),
            death_tx.clone(),
        );
    }
    drop(tx);
    drop(death_tx);

    // Shared (session-wide) state used only for the multi-target bottom
    // line and for the cap notice, which prints at most once overall.
    let mut shared_last_recovery: Instant = session_start;
    let mut shared_last_outage_summary: Option<String> = None;
    let mut cap_notice_printed = false;

    let spark_count = 20usize;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                display.restore_cursor();
                print_summary(&targets, session_start);
                break;
            }
            maybe_dead = death_rx.recv() => {
                // See pinger::spawn's doc comment: while this loop is
                // running, a pinger task ending at all (return or panic)
                // means that target's monitoring silently stopped. Without
                // this, the shared channel only closes once *every* target's
                // task has ended, so one dead target would otherwise leave
                // its row frozen at stale "healthy" state forever while
                // survivors keep rendering. Fail loud instead: restore the
                // terminal (in case the death was a panic that already ran
                // the panic hook and re-enabled line wrap — restoring again
                // here is idempotent) and exit non-zero naming the target,
                // before any further render can run.
                if let Some(idx) = maybe_dead {
                    display.restore_cursor();
                    eprintln!(
                        "\nekg: monitoring for '{}' stopped unexpectedly — exiting.",
                        targets[idx].host
                    );
                    std::process::exit(1);
                }
            }
            maybe_event = rx.recv() => {
                let first = match maybe_event {
                    Some(e) => e,
                    None => break, // all pinger tasks ended
                };

                apply_event(
                    &mut targets,
                    &mut display,
                    &args,
                    multi,
                    &mut shared_last_recovery,
                    &mut shared_last_outage_summary,
                    &mut cap_notice_printed,
                    &mut recorder,
                    first,
                )?;

                // Drain any events that are already waiting so a backlog
                // triggers one redraw per batch instead of one full
                // clear+redraw per event (O(targets) work per event would
                // otherwise become O(targets^2) per interval tick when
                // every target's event lands close together). Outage
                // bookkeeping/emission still happens per event, in order —
                // only the panel redraw below is coalesced.
                while let Ok(tagged) = rx.try_recv() {
                    apply_event(
                        &mut targets,
                        &mut display,
                        &args,
                        multi,
                        &mut shared_last_recovery,
                        &mut shared_last_outage_summary,
                        &mut cap_notice_printed,
                        &mut recorder,
                        tagged,
                    )?;
                }

                if multi {
                    render_multi_panel(&mut display, &targets, session_start, shared_last_recovery, shared_last_outage_summary.as_deref())?;
                } else {
                    let t = &targets[0];
                    let down_for = t.down_since.map(|inst| inst.elapsed());
                    let uptime = if t.down_since.is_some() {
                        None
                    } else {
                        Some(t.last_recovery.elapsed())
                    };

                    display.render_single(
                        &t.host,
                        &t.stats,
                        down_for,
                        uptime,
                        t.last_outage_summary.as_deref(),
                        spark_count,
                    )?;
                }

                // --count: once every target has sent at least that many
                // pings, stop like a scripted one-shot run — print the
                // normal summary and exit with a code that reflects whether
                // loss stayed within --max-loss, so shell scripts can act
                // on it directly instead of scraping the summary text.
                if let Some(n) = args.count {
                    if targets.iter().all(|t| t.stats.sent >= n) {
                        display.restore_cursor();
                        print_summary(&targets, session_start);
                        let ok = targets
                            .iter()
                            .all(|t| loss_within_threshold(t.stats.lifetime_loss_pct(), args.max_loss));
                        std::process::exit(if ok { 0 } else { 1 });
                    }
                }
            }
        }
    }

    Ok(())
}

/// Pure exit-code predicate for `--count` / `--max-loss`: true if `loss_pct`
/// is an acceptable outcome. Split out from the exit path so the "any loss
/// fails by default, <=  max_loss passes" semantics are unit-testable
/// without spinning up the ping loop.
fn loss_within_threshold(loss_pct: f64, max_loss_pct: f64) -> bool {
    loss_pct <= max_loss_pct
}

/// clap `value_parser` for `--max-loss`. A loss percentage is only
/// meaningful in 0.0..=100.0, and it must be finite: `inf` trivially passes
/// `loss_pct <= max_loss_pct` for any loss, and `NaN` fails every
/// comparison (`<=` is always false for NaN), including the ones a caller
/// would expect to be obviously fine (e.g. 0% loss). Either would make
/// `--count`'s exit code silently lie about a failing run, so this rejects
/// both up front with a clear error instead of letting them reach
/// `loss_within_threshold`.
fn parse_max_loss(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("'{s}' is not a number"))?;
    if !v.is_finite() {
        return Err(format!("--max-loss must be a finite number, got '{s}'"));
    }
    if !(0.0..=100.0).contains(&v) {
        return Err(format!(
            "--max-loss must be between 0 and 100 (a percentage), got '{s}'"
        ));
    }
    Ok(v)
}

/// Applies one tagged ping event to its target's state: outage/recovery
/// bookkeeping (including any permanent outage/notice line emission) and
/// the rolling `Stats` record. Split out from the render step so a burst of
/// pending events can all be applied before a single coalesced redraw
/// (non-blocking's #3) — every event must still be applied so no outage
/// transition is ever skipped, only the panel redraw is batched.
#[allow(clippy::too_many_arguments)]
fn apply_event(
    targets: &mut [TargetRuntime],
    display: &mut Display,
    args: &Args,
    multi: bool,
    shared_last_recovery: &mut Instant,
    shared_last_outage_summary: &mut Option<String>,
    cap_notice_printed: &mut bool,
    recorder: &mut Option<Recorder>,
    tagged: TaggedEvent,
) -> std::io::Result<()> {
    let TaggedEvent { idx, event } = tagged;
    let t = &mut targets[idx];

    // --log: one JSONL line per sample, timeout-or-not, before any
    // outage/state bookkeeping below — the recorder's job is a complete,
    // unopinionated record, not just the samples that also affect state.
    if let Some(rec) = recorder.as_mut() {
        let rtt_ms = match event {
            PingEvent::Reply(d) => Some(d.as_secs_f64() * 1000.0),
            PingEvent::Timeout => None,
        };
        rec.log_sample(&t.host, rtt_ms)?;
    }

    match event {
        PingEvent::Reply(_) => {
            if t.consecutive_timeouts >= OUTAGE_THRESHOLD {
                // Outage recovery: this reply ends a declared outage, so
                // uptime resets from here. A reply after only a
                // sub-threshold blip (1-2 timeouts, never reached
                // OUTAGE_THRESHOLD) is NOT a recovery — uptime keeps
                // counting from the session start / last real outage.
                if let Some(started_wall) = t.down_wall_start {
                    let duration = t.down_since.map(|inst| inst.elapsed()).unwrap_or_default();
                    // Log the outage_end record regardless of the display's
                    // --max-outages cap — the JSONL log is meant to be the
                    // complete record for offline analysis, unlike the
                    // terminal's deliberately-truncated outage-line history.
                    if let Some(rec) = recorder.as_mut() {
                        rec.log_outage_end(&t.host, started_wall + duration, duration)?;
                    }
                    match args.max_outages {
                        Some(cap) if t.outage_count >= cap => {
                            if t.outage_count == cap && cap > 0 && !*cap_notice_printed {
                                display.emit_notice_line(&format!(
                                    "… outage log capped at {cap}; later outages still counted in the summary"
                                ))?;
                                *cap_notice_printed = true;
                            }
                        }
                        _ => {
                            let host_prefix = if multi { Some(t.host.as_str()) } else { None };
                            display.emit_outage_line(host_prefix, started_wall, duration)?;
                        }
                    }
                    t.last_outage_summary = Some(format!(
                        "last outage: {} ({})",
                        display::local_hms(started_wall),
                        fmt_short(duration)
                    ));
                    *shared_last_outage_summary = Some(format!(
                        "last outage: {} {} ({})",
                        display::local_hms(started_wall),
                        t.host,
                        fmt_short(duration)
                    ));
                    t.outage_count += 1;
                }
                let now = Instant::now();
                t.last_recovery = now;
                *shared_last_recovery = now;
            }
            t.consecutive_timeouts = 0;
            t.down_since = None;
            t.down_wall_start = None;
        }
        PingEvent::Timeout => {
            t.consecutive_timeouts += 1;
            if t.consecutive_timeouts == OUTAGE_THRESHOLD {
                let wall_now = SystemTime::now();
                t.down_since = Some(Instant::now());
                t.down_wall_start = Some(wall_now);
                if let Some(rec) = recorder.as_mut() {
                    rec.log_outage_start(&t.host, wall_now)?;
                }
            }
        }
    }

    let sample: Sample = event.into();
    targets[idx].stats.record(sample);

    Ok(())
}

/// Builds and renders the multi-target panel: one row per target plus the
/// shared bottom line (session-wide uptime + the most recent outage across
/// any target).
fn render_multi_panel(
    display: &mut Display,
    targets: &[TargetRuntime],
    session_start: Instant,
    shared_last_recovery: Instant,
    shared_last_outage_summary: Option<&str>,
) -> std::io::Result<()> {
    let rows: Vec<MultiTargetRow> = targets
        .iter()
        .map(|t| MultiTargetRow {
            host: &t.host,
            stats: &t.stats,
            down_since: t.down_since.map(|inst| inst.elapsed()),
        })
        .collect();

    let any_down = targets.iter().any(|t| t.down_since.is_some());
    let session_uptime = fmt_short(session_start.elapsed());
    let shared_line = match (any_down, shared_last_outage_summary) {
        (true, Some(outage)) => format!("  session {session_uptime} · down now · {outage}"),
        (true, None) => format!("  session {session_uptime} · down now"),
        (false, Some(outage)) => {
            format!(
                "  up {} · {outage}",
                fmt_short(shared_last_recovery.elapsed())
            )
        }
        (false, None) => format!("  up {}", fmt_short(shared_last_recovery.elapsed())),
    };

    display.render_multi(&rows, &shared_line)
}

fn fmt_short(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn print_summary(targets: &[TargetRuntime], session_start: Instant) {
    let duration = session_start.elapsed();
    println!("\n--- ekg summary ---");
    println!("session duration: {}", fmt_short(duration));
    for t in targets {
        if targets.len() > 1 {
            println!("\n{}:", t.host);
        }
        println!("sent: {}  received: {}", t.stats.sent, t.stats.received);
        println!("loss: {:.1}%", t.stats.lifetime_loss_pct());
        if let (Some(avg), Some(min), Some(max)) =
            (t.stats.avg_rtt(), t.stats.min_rtt, t.stats.max_rtt)
        {
            println!(
                "avg: {}ms  min: {}ms  max: {}ms",
                avg.as_millis(),
                min.as_millis(),
                max.as_millis()
            );
        }
        println!("outages: {}", t.outage_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_within_threshold_default_zero_fails_any_loss() {
        assert!(loss_within_threshold(0.0, 0.0));
        assert!(!loss_within_threshold(0.001, 0.0));
        assert!(!loss_within_threshold(50.0, 0.0));
    }

    #[test]
    fn loss_within_threshold_respects_max_loss() {
        assert!(loss_within_threshold(20.0, 20.0));
        assert!(loss_within_threshold(19.9, 20.0));
        assert!(!loss_within_threshold(20.1, 20.0));
    }

    #[test]
    fn loss_within_threshold_full_loss_and_full_allowance() {
        assert!(loss_within_threshold(100.0, 100.0));
        assert!(!loss_within_threshold(100.0, 99.9));
    }

    #[test]
    fn parse_max_loss_rejects_out_of_range() {
        assert!(parse_max_loss("101").is_err());
        assert!(parse_max_loss("-1").is_err());
        assert!(parse_max_loss("1000").is_err());
    }

    #[test]
    fn parse_max_loss_rejects_non_finite() {
        assert!(parse_max_loss("inf").is_err());
        assert!(parse_max_loss("infinity").is_err());
        assert!(parse_max_loss("-inf").is_err());
        assert!(parse_max_loss("NaN").is_err());
    }

    #[test]
    fn parse_max_loss_rejects_garbage() {
        assert!(parse_max_loss("not-a-number").is_err());
        assert!(parse_max_loss("").is_err());
    }

    #[test]
    fn parse_max_loss_accepts_boundary_and_mid_values() {
        assert_eq!(parse_max_loss("0"), Ok(0.0));
        assert_eq!(parse_max_loss("50"), Ok(50.0));
        assert_eq!(parse_max_loss("100"), Ok(100.0));
        assert_eq!(parse_max_loss("20.5"), Ok(20.5));
    }
}
