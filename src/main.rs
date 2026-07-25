mod display;
mod pinger;
mod stats;

use std::net::{IpAddr, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime};

use clap::Parser;

use display::{Display, OUTAGE_THRESHOLD};
use pinger::PingEvent;
use stats::{Sample, Stats};

/// ekg — compact in-place ping monitor.
#[derive(Parser, Debug)]
#[command(name = "ekg", version, about = "Compact in-place ping monitor")]
struct Args {
    /// Target host or IP to ping.
    #[arg(default_value = "1.1.1.1")]
    host: String,

    /// Ping interval in seconds (fractions allowed, e.g. 0.5).
    #[arg(short, long, default_value_t = 1.0)]
    interval: f64,

    /// Rolling window size (number of samples kept for stats/sparkline).
    #[arg(short, long, default_value_t = 60)]
    window: usize,

    /// Stop printing permanent outage lines after this many (0 disables them
    /// entirely). The panel's "last outage" line and the final summary still
    /// reflect every outage.
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

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();

    let ip = match resolve_host(&args.host) {
        Ok(ip) => ip,
        Err(e) => {
            eprintln!("ekg: could not resolve '{}': {e}", args.host);
            std::process::exit(1);
        }
    };

    let client = match pinger::create_client(ip) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("ekg: {msg}");
            std::process::exit(1);
        }
    };

    let interval = Duration::from_secs_f64(args.interval.max(0.001));
    let mut stats = Stats::new(args.window);
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
    let mut rx = pinger::spawn(client, ip, interval);

    let mut consecutive_timeouts: u32 = 0;
    let mut down_since: Option<Instant> = None;
    let mut down_wall_start: Option<SystemTime> = None;
    let mut last_recovery: Instant = session_start;
    let mut outage_count: u32 = 0;
    let mut last_outage_summary: Option<String> = None;
    let mut cap_notice_printed = false;

    let spark_count = 20usize;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                display.restore_cursor();
                print_summary(&stats, session_start, outage_count);
                break;
            }
            maybe_event = rx.recv() => {
                let event = match maybe_event {
                    Some(e) => e,
                    None => break, // pinger task ended
                };

                match event {
                    PingEvent::Reply(_) => {
                        if consecutive_timeouts >= OUTAGE_THRESHOLD {
                            // Outage recovery: this reply ends a declared
                            // outage, so uptime resets from here. A reply
                            // after only a sub-threshold blip (1-2 timeouts,
                            // never reached OUTAGE_THRESHOLD) is NOT a
                            // recovery — uptime keeps counting from the
                            // session start / last real outage.
                            if let Some(started_wall) = down_wall_start {
                                let duration = down_since
                                    .map(|t| t.elapsed())
                                    .unwrap_or_default();
                                match args.max_outages {
                                    Some(cap) if outage_count >= cap => {
                                        if outage_count == cap && cap > 0 && !cap_notice_printed {
                                            display.emit_notice_line(&format!(
                                                "… outage log capped at {cap}; later outages still counted in the summary"
                                            ))?;
                                            cap_notice_printed = true;
                                        }
                                    }
                                    _ => display.emit_outage_line(started_wall, duration)?,
                                }
                                last_outage_summary = Some(format!(
                                    "last outage: {} ({})",
                                    display::local_hms(started_wall),
                                    fmt_short(duration)
                                ));
                                outage_count += 1;
                            }
                            last_recovery = Instant::now();
                        }
                        consecutive_timeouts = 0;
                        down_since = None;
                        down_wall_start = None;
                    }
                    PingEvent::Timeout => {
                        consecutive_timeouts += 1;
                        if consecutive_timeouts == OUTAGE_THRESHOLD {
                            down_since = Some(Instant::now());
                            down_wall_start = Some(SystemTime::now());
                        }
                    }
                }

                let sample: Sample = event.into();
                stats.record(sample);

                let down_for = down_since.map(|t| t.elapsed());
                let uptime = if down_since.is_some() {
                    None
                } else {
                    Some(last_recovery.elapsed())
                };

                display.render(
                    &args.host,
                    &stats,
                    down_for,
                    uptime,
                    last_outage_summary.as_deref(),
                    spark_count,
                )?;
            }
        }
    }

    Ok(())
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

fn print_summary(stats: &Stats, session_start: Instant, outage_count: u32) {
    let duration = session_start.elapsed();
    println!("\n--- ekg summary ---");
    println!("session duration: {}", fmt_short(duration));
    println!("sent: {}  received: {}", stats.sent, stats.received);
    println!("loss: {:.1}%", stats.lifetime_loss_pct());
    if let (Some(avg), Some(min), Some(max)) = (stats.avg_rtt(), stats.min_rtt, stats.max_rtt) {
        println!(
            "avg: {}ms  min: {}ms  max: {}ms",
            avg.as_millis(),
            min.as_millis(),
            max.as_millis()
        );
    }
    println!("outages: {outage_count}");
}
