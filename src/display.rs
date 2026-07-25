//! In-place crossterm panel renderer: a 2-3 line status panel that redraws
//! itself each tick, plus permanent one-line outage records that scroll up
//! into terminal history above the panel.

use std::io::{stdout, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, DisableLineWrap, EnableLineWrap},
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

/// Returns the first candidate string that fits `width` columns, falling back
/// to the last (tersest) candidate when none fit. Used for progressive
/// shortening of panel lines in narrow panes.
fn first_fit(width: usize, candidates: &[String]) -> String {
    candidates
        .iter()
        .find(|c| c.chars().count() <= width)
        .or_else(|| candidates.last())
        .cloned()
        .unwrap_or_default()
}

/// Default sparkline width for a multi-target row before narrow-pane
/// shrinking kicks in. Rows are more crowded than the single-target panel
/// (host column + last-ms + avg/loss share the line), so this starts
/// smaller than the single-target `spark_count` passed in from `main`.
const SPARK_TARGET_COUNT: usize = 10;

/// One target's live state, as needed to render its multi-target row.
pub struct MultiTargetRow<'a> {
    pub host: &'a str,
    pub stats: &'a Stats,
    pub down_since: Option<Duration>,
}

/// Computes the column width needed to left-align a set of host labels so
/// the values that follow them line up across rows. Pure and unit-tested.
pub fn host_column_width(hosts: &[&str]) -> usize {
    hosts.iter().map(|h| h.chars().count()).max().unwrap_or(0)
}

/// Candidate row strings (widest-first) for an up/reachable target, used
/// with `first_fit` for narrow-pane shortening: full detail (host padded to
/// the column width, for cross-row alignment) first, then progressively
/// shorter forms. Column alignment is a wide-pane luxury — the shortened
/// candidates use the raw (unpadded) `host` so one long hostname among the
/// targets doesn't defeat narrow-pane shortening for every row. Pure and
/// unit-tested.
pub fn up_row_candidates(
    host: &str,
    host_padded: &str,
    last_ms: &str,
    avg: &str,
    loss_pct: f64,
    spark: &str,
) -> Vec<String> {
    vec![
        format!("{host_padded}  {last_ms}   avg {avg} · {loss_pct:.0}% · {spark}"),
        format!("{host_padded}  {last_ms}   {spark}"),
        format!("{host} {last_ms} {spark}"),
    ]
}

/// Candidate row strings (widest-first) for a down/unreachable target. Same
/// alignment-is-a-wide-pane-luxury rule as `up_row_candidates`: only the
/// full candidate pads `host` to the column width. Pure and unit-tested.
pub fn down_row_candidates(host: &str, host_padded: &str, duration_str: &str) -> Vec<String> {
    vec![
        format!("{host_padded}  no reply for {duration_str}"),
        format!("{host}  no reply {duration_str}"),
    ]
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

    /// Terminal setup for in-place rendering: hides the cursor AND disables
    /// line wrapping. Wrap must be off while the panel is live — a wrapped
    /// panel line occupies more physical rows than the renderer's logical
    /// line count, so cursor-up repositioning drifts and the panel smears
    /// duplicate rows down the screen in narrow terminals.
    pub fn hide_cursor(&mut self) -> std::io::Result<()> {
        stdout()
            .queue(cursor::Hide)?
            .queue(DisableLineWrap)?
            .flush()?;
        self.hidden_cursor = true;
        Ok(())
    }

    pub fn restore_cursor(&mut self) {
        if self.hidden_cursor {
            let _ = stdout()
                .queue(cursor::Show)
                .and_then(|s| s.queue(EnableLineWrap))
                .and_then(|s| s.flush());
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
    /// above where the panel will resume. `host` is `None` in single-target
    /// mode (output is byte-identical to before multi-target support) and
    /// `Some(host)` in multi-target mode, which prefixes the line with the
    /// host so outage history stays attributable per-target.
    pub fn emit_outage_line(
        &mut self,
        host: Option<&str>,
        start_wall: std::time::SystemTime,
        duration: Duration,
    ) -> std::io::Result<()> {
        self.clear_panel()?;
        let start_local = local_hms(start_wall);
        let end_local = local_hms(start_wall + duration);
        let mut out = stdout();
        out.queue(SetForegroundColor(Color::Red))?;
        let line = match host {
            Some(h) => format!(
                "✗ {h}  outage {start_local} → {end_local} ({})\n",
                fmt_duration_short(duration)
            ),
            None => format!(
                "✗ outage {start_local} → {end_local} ({})\n",
                fmt_duration_short(duration)
            ),
        };
        out.queue(Print(line))?;
        out.queue(ResetColor)?;
        out.flush()?;
        Ok(())
    }

    /// Prints a permanent dimmed notice line (e.g. "outage log capped") that
    /// scrolls into history like an outage line.
    pub fn emit_notice_line(&mut self, text: &str) -> std::io::Result<()> {
        self.clear_panel()?;
        let mut out = stdout();
        out.queue(SetForegroundColor(Color::DarkGrey))?;
        out.queue(Print(format!("{text}\n")))?;
        out.queue(ResetColor)?;
        out.flush()?;
        Ok(())
    }

    /// Renders the (up to) 3-line single-target panel in place. Output is
    /// byte-identical in structure to before multi-target support — this is
    /// the common case and must not regress.
    #[allow(clippy::too_many_arguments)]
    pub fn render_single(
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

        let width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
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
            let d = fmt_duration_short(down_for);
            let line = first_fit(
                width,
                &[
                    format!("✗ {host}   no reply for {d}"),
                    format!("✗ no reply {d}"),
                ],
            );
            out.queue(SetForegroundColor(color))?;
            out.queue(Print(format!("{line}\n")))?;
            out.queue(ResetColor)?;
        } else {
            let last_ms = avg.map(fmt_ms).unwrap_or_else(|| "--".to_string());

            // Shrink the sparkline to the columns actually available so the
            // panel stays useful (not clipped) in narrow panes. Wrap is
            // disabled, so an over-long line would clip, never corrupt.
            let prefix = format!(" {host}   {last_ms}   ");
            let available = width.saturating_sub(prefix.chars().count() + 2);
            let effective_spark = spark_count.min(available);
            let buckets = stats.sparkline_buckets(effective_spark.max(1));

            out.queue(SetForegroundColor(color))?;
            out.queue(Print("●"))?;
            out.queue(ResetColor)?;
            out.queue(Print(prefix))?;
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

        // Line 2 — progressively shortened in narrow panes: jitter goes
        // first, then labels; avg / loss / uptime survive the longest.
        let avg_str = avg.map(fmt_ms).unwrap_or_else(|| "--".to_string());
        let jitter_str = stats
            .jitter()
            .map(fmt_ms)
            .unwrap_or_else(|| "--".to_string());
        let uptime_str = uptime_since_recovery
            .map(fmt_duration_short)
            .unwrap_or_else(|| "--".to_string());
        let line2 = first_fit(
            width,
            &[
                format!(
                    "  avg {avg_str} · jitter {jitter_str} · loss {loss:.0}% · up {uptime_str}"
                ),
                format!("  avg {avg_str} · loss {loss:.0}% · up {uptime_str}"),
                format!("  {avg_str} · {loss:.0}% · up {uptime_str}"),
                format!("  {avg_str} {loss:.0}% {uptime_str}"),
            ],
        );
        out.queue(Print(format!("{line2}\n")))?;

        let mut height = 2;

        // Line 3 (only after an outage has occurred), shortened when narrow.
        if let Some(summary) = last_outage_summary {
            let line3 = first_fit(
                width,
                &[
                    format!("  {summary}"),
                    format!("  {}", summary.replacen("last outage: ", "outage ", 1)),
                ],
            );
            out.queue(Print(format!("{line3}\n")))?;
            height = 3;
        }

        out.flush()?;
        self.last_height = height;
        Ok(())
    }

    /// Renders the multi-target panel: one condensed row per host, plus a
    /// shared bottom line with session-wide info. Only used when there is
    /// more than one target — the single-target format above is untouched.
    pub fn render_multi(
        &mut self,
        rows: &[MultiTargetRow],
        shared_line: &str,
    ) -> std::io::Result<()> {
        self.clear_panel()?;
        let mut out = stdout();

        let width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
        let host_width = host_column_width(&rows.iter().map(|r| r.host).collect::<Vec<_>>());

        for row in rows {
            let host_padded = format!("{:<width$}", row.host, width = host_width);
            let is_down = row.down_since.is_some();
            let avg = row.stats.avg_rtt();
            let avg_ms = avg.map(|d| d.as_secs_f64() * 1000.0);
            let loss = row.stats.loss_pct();
            let q = quality(avg_ms, loss, is_down);
            let color = color_for(q);

            if let Some(down_for) = row.down_since {
                let d = fmt_duration_short(down_for);
                let candidates = down_row_candidates(row.host, &host_padded, &d);
                let line = first_fit(width.saturating_sub(2), &candidates);
                out.queue(SetForegroundColor(color))?;
                out.queue(Print(format!("✗ {line}\n")))?;
                out.queue(ResetColor)?;
            } else {
                // "Current" is the most recent reply RTT, not the rolling
                // average — they read the same in a flat window but must
                // diverge whenever recent pings vary or a timeout dropped
                // out of the average's window.
                let last_ms = row
                    .stats
                    .last_reply_rtt()
                    .map(fmt_ms)
                    .unwrap_or_else(|| "--".to_string());
                let avg_str = avg.map(fmt_ms).unwrap_or_else(|| "--".to_string());

                // Shrink the sparkline to the columns actually available,
                // mirroring the single-target narrow-pane approach.
                let prefix_len = 2 + host_padded.chars().count() + 2 + last_ms.chars().count() + 3;
                let available = width.saturating_sub(prefix_len + 2);
                let spark_full = SPARK_TARGET_COUNT.min(available);
                let buckets = row.stats.sparkline_buckets(spark_full.max(1));
                let spark: String = buckets
                    .iter()
                    .map(|b| match b {
                        Some(level) => SPARK_CHARS[*level as usize],
                        None => TIMEOUT_GAP_CHAR,
                    })
                    .collect();

                let candidates =
                    up_row_candidates(row.host, &host_padded, &last_ms, &avg_str, loss, &spark);
                let line = first_fit(width.saturating_sub(2), &candidates);
                out.queue(SetForegroundColor(color))?;
                out.queue(Print("● "))?;
                out.queue(ResetColor)?;
                out.queue(Print(format!("{line}\n")))?;
            }
        }

        out.queue(Print(format!("{shared_line}\n")))?;

        out.flush()?;
        self.last_height = rows.len() as u16 + 1;
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

#[cfg(test)]
mod tests {
    use super::first_fit;

    fn cands() -> Vec<String> {
        vec![
            "  avg 14ms · jitter 2ms · loss 0% · up 2h14m".into(),
            "  avg 14ms · loss 0% · up 2h14m".into(),
            "  14ms · 0% · up 2h14m".into(),
            "  14ms 0% 2h14m".into(),
        ]
    }

    #[test]
    fn wide_pane_gets_full_line() {
        assert_eq!(first_fit(80, &cands()), cands()[0]);
    }

    #[test]
    fn medium_pane_drops_jitter() {
        assert_eq!(first_fit(35, &cands()), cands()[1]);
    }

    #[test]
    fn narrow_pane_drops_labels() {
        assert_eq!(first_fit(23, &cands()), cands()[2]);
    }

    #[test]
    fn tiny_pane_falls_back_to_tersest_even_if_it_clips() {
        assert_eq!(first_fit(5, &cands()), cands()[3]);
    }

    #[test]
    fn empty_candidates_yield_empty_string() {
        assert_eq!(first_fit(10, &[]), "");
    }
}

#[cfg(test)]
mod multi_target_tests {
    use super::{down_row_candidates, host_column_width, up_row_candidates};

    #[test]
    fn host_column_width_picks_the_longest() {
        assert_eq!(host_column_width(&["1.1.1.1", "192.168.1.1", "a"]), 11);
    }

    #[test]
    fn host_column_width_empty_is_zero() {
        assert_eq!(host_column_width(&[]), 0);
    }

    #[test]
    fn host_column_width_single_host() {
        assert_eq!(host_column_width(&["router.local"]), 12);
    }

    #[test]
    fn up_row_full_candidate_has_avg_and_loss() {
        let width = host_column_width(&["1.1.1.1", "192.168.1.1"]);
        let host_padded = format!("{:<width$}", "1.1.1.1", width = width);
        let candidates = up_row_candidates("1.1.1.1", &host_padded, "12ms", "14ms", 0.0, "▁▂▂▃▂");
        assert_eq!(
            candidates[0],
            format!("{host_padded}  12ms   avg 14ms · 0% · ▁▂▂▃▂")
        );
        assert!(candidates[0].contains("avg 14ms"));
        assert!(candidates[0].contains("0%"));
    }

    #[test]
    fn up_row_short_candidate_drops_avg_and_loss() {
        let host_padded = "1.1.1.1".to_string();
        let candidates = up_row_candidates("1.1.1.1", &host_padded, "12ms", "14ms", 0.0, "▁▂▂▃▂");
        assert_eq!(candidates[1], format!("{host_padded}  12ms   ▁▂▂▃▂"));
        assert!(!candidates[1].contains("avg"));
        assert!(!candidates[1].contains('%'));
    }

    #[test]
    fn up_row_tersest_candidate_drops_host_padding() {
        // A long hostname among the targets pads `host_padded` far beyond
        // the raw host — the tersest candidate must use the raw host so a
        // short-named row doesn't inherit the long one's width penalty.
        let width = host_column_width(&["a", "a-very-long-router-hostname.local"]);
        let host_padded = format!("{:<width$}", "a", width = width);
        assert!(
            host_padded.len() > 1,
            "padded host should be wider than raw"
        );
        let candidates = up_row_candidates("a", &host_padded, "12ms", "14ms", 0.0, "▁▂▂▃▂");
        assert_eq!(candidates[2], "a 12ms ▁▂▂▃▂");
        assert!(candidates[2].len() < host_padded.len() + 20);
    }

    #[test]
    fn down_row_full_candidate_has_for() {
        let candidates = down_row_candidates("192.168.1.1", "192.168.1.1", "12s");
        assert_eq!(candidates[0], "192.168.1.1  no reply for 12s");
    }

    #[test]
    fn down_row_short_candidate_drops_for_and_padding() {
        let width = host_column_width(&["192.168.1.1", "a-very-long-router-hostname.local"]);
        let host_padded = format!("{:<width$}", "192.168.1.1", width = width);
        let candidates = down_row_candidates("192.168.1.1", &host_padded, "12s");
        assert_eq!(candidates[1], "192.168.1.1  no reply 12s");
        assert!(candidates[1].len() < host_padded.len() + 20);
    }
}
