mod display;
mod pinger;
mod stats;

use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;

use display::{Display, MultiTargetRow, OUTAGE_THRESHOLD};
use pinger::{PingEvent, TaggedEvent};
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
    for (idx, ((_, ip), client)) in resolved.iter().zip(clients.into_iter()).enumerate() {
        pinger::spawn(client, *ip, interval, idx, tx.clone());
    }
    drop(tx);

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
            maybe_event = rx.recv() => {
                let tagged = match maybe_event {
                    Some(e) => e,
                    None => break, // all pinger tasks ended
                };
                let TaggedEvent { idx, event } = tagged;
                let t = &mut targets[idx];

                match event {
                    PingEvent::Reply(_) => {
                        if t.consecutive_timeouts >= OUTAGE_THRESHOLD {
                            // Outage recovery: this reply ends a declared
                            // outage, so uptime resets from here. A reply
                            // after only a sub-threshold blip (1-2 timeouts,
                            // never reached OUTAGE_THRESHOLD) is NOT a
                            // recovery — uptime keeps counting from the
                            // session start / last real outage.
                            if let Some(started_wall) = t.down_wall_start {
                                let duration = t.down_since
                                    .map(|inst| inst.elapsed())
                                    .unwrap_or_default();
                                match args.max_outages {
                                    Some(cap) if t.outage_count >= cap => {
                                        if t.outage_count == cap && cap > 0 && !cap_notice_printed {
                                            display.emit_notice_line(&format!(
                                                "… outage log capped at {cap}; later outages still counted in the summary"
                                            ))?;
                                            cap_notice_printed = true;
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
                                shared_last_outage_summary = Some(format!(
                                    "last outage: {} {} ({})",
                                    display::local_hms(started_wall),
                                    t.host,
                                    fmt_short(duration)
                                ));
                                t.outage_count += 1;
                            }
                            let now = Instant::now();
                            t.last_recovery = now;
                            shared_last_recovery = now;
                        }
                        t.consecutive_timeouts = 0;
                        t.down_since = None;
                        t.down_wall_start = None;
                    }
                    PingEvent::Timeout => {
                        t.consecutive_timeouts += 1;
                        if t.consecutive_timeouts == OUTAGE_THRESHOLD {
                            t.down_since = Some(Instant::now());
                            t.down_wall_start = Some(SystemTime::now());
                        }
                    }
                }

                let sample: Sample = event.into();
                targets[idx].stats.record(sample);

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
            }
        }
    }

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
