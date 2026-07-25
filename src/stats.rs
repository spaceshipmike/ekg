//! Rolling-window ping statistics: average RTT, jitter (mean absolute
//! deviation), packet loss percentage, and sparkline bucket data.

use std::collections::VecDeque;
use std::time::Duration;

/// One outcome for a single ping attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sample {
    Reply(Duration),
    Timeout,
}

/// Fixed-size ring buffer of the most recent `window` samples, plus
/// derived rolling statistics.
pub struct Stats {
    window: usize,
    samples: VecDeque<Sample>,
    // Lifetime (whole-session) counters, independent of the rolling window.
    pub sent: u64,
    pub received: u64,
    pub min_rtt: Option<Duration>,
    pub max_rtt: Option<Duration>,
}

impl Stats {
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        Self {
            window,
            samples: VecDeque::with_capacity(window),
            sent: 0,
            received: 0,
            min_rtt: None,
            max_rtt: None,
        }
    }

    pub fn record(&mut self, sample: Sample) {
        self.sent += 1;
        if let Sample::Reply(rtt) = sample {
            self.received += 1;
            self.min_rtt = Some(self.min_rtt.map_or(rtt, |m| m.min(rtt)));
            self.max_rtt = Some(self.max_rtt.map_or(rtt, |m| m.max(rtt)));
        }
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The most recent reply RTT in the rolling window — i.e. the "current"
    /// reading, as distinct from `avg_rtt`'s rolling average. Skips over any
    /// trailing timeouts to find the last successful reply; `None` only if
    /// there is no reply anywhere in the window (e.g. every sample so far
    /// has timed out).
    pub fn last_reply_rtt(&self) -> Option<Duration> {
        self.samples.iter().rev().find_map(|s| match s {
            Sample::Reply(d) => Some(*d),
            Sample::Timeout => None,
        })
    }

    /// Average RTT over the rolling window (replies only). `None` if no
    /// replies are in the window.
    pub fn avg_rtt(&self) -> Option<Duration> {
        let replies: Vec<Duration> = self
            .samples
            .iter()
            .filter_map(|s| match s {
                Sample::Reply(d) => Some(*d),
                Sample::Timeout => None,
            })
            .collect();
        if replies.is_empty() {
            return None;
        }
        let total: Duration = replies.iter().sum();
        Some(total / replies.len() as u32)
    }

    /// Jitter over the rolling window as the mean absolute deviation of
    /// RTTs from their mean. `None` if fewer than one reply is present.
    pub fn jitter(&self) -> Option<Duration> {
        let replies: Vec<f64> = self
            .samples
            .iter()
            .filter_map(|s| match s {
                Sample::Reply(d) => Some(d.as_secs_f64()),
                Sample::Timeout => None,
            })
            .collect();
        if replies.is_empty() {
            return None;
        }
        let mean = replies.iter().sum::<f64>() / replies.len() as f64;
        let mad = replies.iter().map(|r| (r - mean).abs()).sum::<f64>() / replies.len() as f64;
        Some(Duration::from_secs_f64(mad.max(0.0)))
    }

    /// Packet loss percentage over the rolling window (0.0 if window empty).
    pub fn loss_pct(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let timeouts = self
            .samples
            .iter()
            .filter(|s| matches!(s, Sample::Timeout))
            .count();
        (timeouts as f64 / self.samples.len() as f64) * 100.0
    }

    /// Lifetime packet loss percentage (sent vs received), for the final
    /// session summary.
    pub fn lifetime_loss_pct(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        ((self.sent - self.received) as f64 / self.sent as f64) * 100.0
    }

    /// Sparkline bucket levels (0..=7) for the last `count` samples, scaled
    /// to fixed absolute RTT thresholds (see [`SPARK_BUCKET_MS`]) rather than
    /// the window's own min/max. This makes bar height mean something
    /// consistent — the same RTT always maps to the same bar — so a single
    /// outlier can't flatten the rest of the window, and post-outage bars
    /// appear at their true level immediately instead of waiting for the
    /// outlier to age out. `None` entries represent timeouts (rendered as a
    /// gap character by the display layer).
    pub fn sparkline_buckets(&self, count: usize) -> Vec<Option<u8>> {
        let start = self.samples.len().saturating_sub(count);
        let recent: Vec<&Sample> = self.samples.iter().skip(start).collect();

        recent
            .iter()
            .map(|s| match s {
                Sample::Timeout => None,
                Sample::Reply(d) => Some(spark_bucket_level(d.as_secs_f64() * 1000.0)),
            })
            .collect()
    }
}

/// Upper bound (in milliseconds, exclusive) of each sparkline bucket level,
/// indexed 0..=6; level 7 is everything at or above the last threshold. The
/// single source of truth for the fixed, log-spaced RTT bands used by
/// [`Stats::sparkline_buckets`] — bar height is absolute badness, not
/// relative to whatever happens to be in the current window.
const SPARK_BUCKET_MS: [f64; 7] = [15.0, 30.0, 60.0, 100.0, 200.0, 400.0, 800.0];

/// Map an RTT (in milliseconds) to its fixed sparkline bucket level (0..=7).
fn spark_bucket_level(rtt_ms: f64) -> u8 {
    SPARK_BUCKET_MS
        .iter()
        .position(|&threshold| rtt_ms < threshold)
        .map_or(7u8, |i| i as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn avg_of_replies_only() {
        let mut s = Stats::new(10);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Reply(ms(20)));
        s.record(Sample::Timeout);
        // avg should ignore the timeout
        let avg = s.avg_rtt().unwrap();
        assert_eq!(avg, ms(15));
    }

    #[test]
    fn avg_none_when_all_timeouts() {
        let mut s = Stats::new(5);
        s.record(Sample::Timeout);
        s.record(Sample::Timeout);
        assert!(s.avg_rtt().is_none());
    }

    #[test]
    fn jitter_zero_for_constant_rtt() {
        let mut s = Stats::new(5);
        for _ in 0..5 {
            s.record(Sample::Reply(ms(20)));
        }
        assert_eq!(s.jitter().unwrap(), Duration::from_secs_f64(0.0));
    }

    #[test]
    fn jitter_mean_abs_deviation() {
        // Samples: 10, 20, 30 ms -> mean 20ms, MAD = (10+0+10)/3 = 6.666...ms
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Reply(ms(20)));
        s.record(Sample::Reply(ms(30)));
        let jitter = s.jitter().unwrap();
        let expected_ms = 6.666_666_666_666_667;
        assert!((jitter.as_secs_f64() * 1000.0 - expected_ms).abs() < 0.001);
    }

    #[test]
    fn loss_pct_basic() {
        let mut s = Stats::new(4);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Timeout);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Timeout);
        assert_eq!(s.loss_pct(), 50.0);
    }

    #[test]
    fn loss_pct_zero_when_empty() {
        let s = Stats::new(4);
        assert_eq!(s.loss_pct(), 0.0);
    }

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut s = Stats::new(2);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Reply(ms(20)));
        s.record(Sample::Reply(ms(30)));
        // window size 2: only 20ms and 30ms should remain -> avg 25ms
        assert_eq!(s.avg_rtt().unwrap(), ms(25));
    }

    #[test]
    fn lifetime_counters_independent_of_window() {
        let mut s = Stats::new(1);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Timeout);
        s.record(Sample::Reply(ms(10)));
        assert_eq!(s.sent, 3);
        assert_eq!(s.received, 2);
        assert!((s.lifetime_loss_pct() - 33.333_333_333_333_336).abs() < 0.0001);
    }

    #[test]
    fn sparkline_all_timeouts_are_none() {
        let mut s = Stats::new(5);
        s.record(Sample::Timeout);
        s.record(Sample::Timeout);
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![None, None]);
    }

    #[test]
    fn sparkline_single_sample_absolute_level() {
        // 15ms is exactly the level-0/1 boundary (< 15 is level 0), so 15ms
        // itself lands in level 1, not a window-relative "mid" bucket.
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(15)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(1)]);
    }

    #[test]
    fn sparkline_uses_absolute_thresholds_not_window_min_max() {
        // Previously this pair would have scaled to Some(0), Some(7) purely
        // because they're the window's min/max. Under absolute scaling, 0ms
        // is level 0 and 100ms lands in its own absolute band (< 200 -> 4).
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(0)));
        s.record(Sample::Reply(ms(100)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(0), Some(4)]);
    }

    #[test]
    fn sparkline_respects_count_window() {
        let mut s = Stats::new(10);
        for i in 0..10 {
            s.record(Sample::Reply(ms(i)));
        }
        let buckets = s.sparkline_buckets(3);
        assert_eq!(buckets.len(), 3);
    }

    #[test]
    fn sparkline_steady_low_latency_window_stays_low() {
        // A steady, healthy window (12-18ms) should render entirely in the
        // bottom two bands (0 or 1), never inflated by relative scaling.
        let mut s = Stats::new(10);
        for ms_val in [12, 14, 15, 16, 18, 13, 17, 14] {
            s.record(Sample::Reply(ms(ms_val)));
        }
        let buckets = s.sparkline_buckets(10);
        assert!(buckets.iter().all(|b| matches!(b, Some(0) | Some(1))));
    }

    #[test]
    fn sparkline_high_rtt_window_hits_level_six() {
        // 731ms falls in the < 800ms band -> level 6, distinct from the
        // ">= 800ms" level 7 band.
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(731)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(6)]);
    }

    #[test]
    fn sparkline_at_or_above_800ms_is_level_seven() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(800)));
        s.record(Sample::Reply(ms(2500)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(7), Some(7)]);
    }

    #[test]
    fn sparkline_recovery_window_straggler_does_not_flatten_steady_bars() {
        // A single 900ms straggler mixed into an otherwise steady, healthy
        // 15ms window must NOT drag the steady samples up (or flatten them
        // to a full-height bar's worth of "badness") the way relative
        // min/max scaling used to. The straggler alone is level 7; the
        // steady samples stay at level 0.
        let mut s = Stats::new(10);
        s.record(Sample::Reply(ms(900)));
        for _ in 0..5 {
            s.record(Sample::Reply(ms(14)));
        }
        let buckets = s.sparkline_buckets(10);
        assert_eq!(buckets[0], Some(7));
        for level in &buckets[1..] {
            assert_eq!(*level, Some(0));
        }
    }

    #[test]
    fn last_reply_rtt_is_most_recent_reply() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(10)));
        s.record(Sample::Reply(ms(20)));
        assert_eq!(s.last_reply_rtt().unwrap(), ms(20));
    }

    #[test]
    fn last_reply_rtt_skips_trailing_timeouts() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(30)));
        s.record(Sample::Timeout);
        s.record(Sample::Timeout);
        // The rolling average would be 30ms too (only reply in window), but
        // last_reply_rtt must independently walk from the back rather than
        // reuse avg_rtt — this test would still pass a buggy alias, so the
        // display-layer regression test (not here) is what actually pins
        // "current != avg"; this test just pins the timeout-skipping search.
        assert_eq!(s.last_reply_rtt().unwrap(), ms(30));
    }

    #[test]
    fn last_reply_rtt_none_when_all_timeouts() {
        let mut s = Stats::new(5);
        s.record(Sample::Timeout);
        s.record(Sample::Timeout);
        assert!(s.last_reply_rtt().is_none());
    }

    #[test]
    fn min_max_track_replies_only() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(50)));
        s.record(Sample::Timeout);
        s.record(Sample::Reply(ms(5)));
        s.record(Sample::Reply(ms(200)));
        assert_eq!(s.min_rtt.unwrap(), ms(5));
        assert_eq!(s.max_rtt.unwrap(), ms(200));
    }
}
