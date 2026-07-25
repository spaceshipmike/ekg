//! In-place crossterm panel renderer: a 2-3 line status panel that redraws
//! itself each tick, plus permanent one-line outage records that scroll up
//! into terminal history above the panel.

use std::io::{stdout, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{Clear, ClearType},
    QueueableCommand,
};

use crate::stats::Stats;

// --- Quality color thresholds (single source of truth) ---------------------
/// Below this average RTT, with zero loss in the rolling window, the
/// connection reads "green". At/above it (but not down/heavy-loss) it reads
/// "yellow" (elevated latency).
pub const GOOD_AVG_MS: f64 = 60.0;
/// At/above this rolling loss % the connection reads "yellow" (minor loss).
pub const MINOR_LOSS_PCT: f64 = 1.0;
/// At/above this rolling loss % the connection reads "red" (heavy loss).
pub const HEAVY_LOSS_PCT: f64 = 20.0;
/// Consecutive timeouts before an outage is declared.
pub const OUTAGE_THRESHOLD: u32 = 3;

const SPARK_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const TIMEOUT_GAP_CHAR: char = '×';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Good,
    Elevated,
    Down,
}

fn quality(avg_ms: Option<f64>, loss_pct: f64, is_down: bool) -> Quality {
    if is_down || loss_pct >= HEAVY_LOSS_PCT {
        return Quality::Down;
    }
    let avg_elevated = avg_ms.map(|a| a >= GOOD_AVG_MS).unwrap_or(false);
    if avg_elevated || loss_pct >= MINOR_LOSS_PCT {
        return Quality::Elevated;
    }
    Quality::Good
}

fn color_for(q: Quality) -> Color {
    match q {
        Quality::Good => Color::Green,
        Quality::Elevated => Color::Yellow,
        Quality::Down => Color::Red,
    }
}

fn fmt_ms(d: Duration) -> String {
    format!("{}ms", d.as_millis())
}

fn fmt_duration_short(d: Duration) -> String {
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

/// Renders the status panel in place and manages outage-line emission.
pub struct Display {
    last_height: u16,
    hidden_cursor: bool,
}

impl Display {
    pub fn new() -> Self {
        Self {
            last_height: 0,
            hidden_cursor: false,
        }
    }

    pub fn hide_cursor(&mut self) -> std::io::Result<()> {
        stdout().queue(cursor::Hide)?.flush()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn restore_cursor(&mut self) {
        if self.hidden_cursor {
            let _ = stdout().queue(cursor::Show).and_then(|s| s.flush());
            self.hidden_cursor = false;
        }
    }

    /// Clears the previously drawn panel (if any) so new lines (an outage
    /// record, or the next render) start from a clean slot.
    fn clear_panel(&mut self) -> std::io::Result<()> {
        let mut out = stdout();
        if self.last_height > 0 {
            out.queue(cursor::MoveUp(self.last_height))?;
            for _ in 0..self.last_height {
                out.queue(Clear(ClearType::CurrentLine))?;
                out.queue(cursor::MoveDown(1))?;
            }
            out.queue(cursor::MoveUp(self.last_height))?;
        }
        out.flush()?;
        self.last_height = 0;
        Ok(())
    }

    /// Prints a permanent outage summary line that scrolls into history,
    /// above where the panel will resume.
    pub fn emit_outage_line(&mut self, start_wall: std::time::SystemTime, duration: Duration) -> std::io::Result<()> {
        self.clear_panel()?;
        let start_local = local_hms(start_wall);
        let end_local = local_hms(start_wall + duration);
        let mut out = stdout();
        out.queue(SetForegroundColor(Color::Red))?;
        out.queue(Print(format!(
            "✗ outage {start_local} → {end_local} ({})\n",
            fmt_duration_short(duration)
        )))?;
        out.queue(ResetColor)?;
        out.flush()?;
        Ok(())
    }

    /// Renders the (up to) 3-line panel in place.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        host: &str,
        stats: &Stats,
        down_since: Option<Duration>,
        uptime_since_recovery: Option<Duration>,
        last_outage_summary: Option<&str>,
        spark_count: usize,
    ) -> std::io::Result<()> {
        self.clear_panel()?;
        let mut out = stdout();

        let is_down = down_since.is_some();
        let avg = stats.avg_rtt();
        let avg_ms = avg.map(|d| d.as_secs_f64() * 1000.0);
        let loss = stats.loss_pct();
        let q = quality(avg_ms, loss, is_down);
        let color = color_for(q);

        // Line 1: only the status dot is colored by quality; the timeout gap
        // character within the sparkline is colored red regardless of
        // overall quality (a visible drop even in an otherwise-green window).
        if let Some(down_for) = down_since {
            out.queue(SetForegroundColor(color))?;
            out.queue(Print(format!(
                "✗ {host}   no reply for {}\n",
                fmt_duration_short(down_for)
            )))?;
            out.queue(ResetColor)?;
        } else {
            let last_ms = avg.map(fmt_ms).unwrap_or_else(|| "--".to_string());
            let buckets = stats.sparkline_buckets(spark_count);

            out.queue(SetForegroundColor(color))?;
            out.queue(Print("●"))?;
            out.queue(ResetColor)?;
            out.queue(Print(format!(" {host}   {last_ms}   ")))?;
            for b in &buckets {
                match b {
                    Some(level) => {
                        out.queue(Print(SPARK_CHARS[*level as usize]))?;
                    }
                    None => {
                        out.queue(SetForegroundColor(Color::Red))?;
                        out.queue(Print(TIMEOUT_GAP_CHAR))?;
                        out.queue(ResetColor)?;
                    }
                }
            }
            out.queue(Print("\n"))?;
        }

        // Line 2
        let avg_str = avg.map(fmt_ms).unwrap_or_else(|| "--".to_string());
        let jitter_str = stats
            .jitter()
            .map(fmt_ms)
            .unwrap_or_else(|| "--".to_string());
        let uptime_str = uptime_since_recovery
            .map(fmt_duration_short)
            .unwrap_or_else(|| "--".to_string());
        out.queue(Print(format!(
            "  avg {avg_str} · jitter {jitter_str} · loss {loss:.0}% · up {uptime_str}\n"
        )))?;

        let mut height = 2;

        // Line 3 (only after an outage has occurred)
        if let Some(summary) = last_outage_summary {
            out.queue(Print(format!("  {summary}\n")))?;
            height = 3;
        }

        out.flush()?;
        self.last_height = height;
        Ok(())
    }
}

impl Default for Display {
    fn default() -> Self {
        Self::new()
    }
}

// Local wall-clock formatting (HH:MM:SS), avoiding an extra crate dependency
// (e.g. chrono) just for this. On Unix (macOS + Linux) this uses the platform
// libc's `localtime_r` via a minimal FFI binding — the C `tm`/`time_t` layout
// used here (with the `tm_gmtoff`/`tm_zone` glibc/BSD extension fields) is
// shared by both Darwin and glibc on 64-bit targets, so one binding covers
// both required platforms. Windows (best-effort only) falls back to UTC
// rather than linking a POSIX-only symbol, so compilation still succeeds
// there even though the displayed clock won't be timezone-adjusted.
#[cfg(unix)]
mod local_time {
    #[repr(C)]
    struct CTm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const std::os::raw::c_char,
    }

    extern "C" {
        fn localtime_r(time: *const i64, result: *mut CTm) -> *mut CTm;
    }

    pub fn local_hms(t: std::time::SystemTime) -> String {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut tm: CTm = unsafe { std::mem::zeroed() };
        unsafe {
            localtime_r(&secs, &mut tm);
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

#[cfg(not(unix))]
mod local_time {
    /// Best-effort fallback (UTC, not local) so non-Unix targets still build.
    pub fn local_hms(t: std::time::SystemTime) -> String {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let h = (secs / 3600) % 24;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// Formats a `SystemTime` as local-timezone `HH:MM:SS` on Unix (macOS +
/// Linux); UTC fallback elsewhere. Shared by the panel (outage lines) and
/// `main.rs` (the final Ctrl-C summary), so there is one source of truth.
pub fn local_hms(t: std::time::SystemTime) -> String {
    local_time::local_hms(t)
}
