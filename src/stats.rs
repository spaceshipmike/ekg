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
    /// to the min/max RTT within that slice. `None` entries represent
    /// timeouts (rendered as a gap character by the display layer).
    pub fn sparkline_buckets(&self, count: usize) -> Vec<Option<u8>> {
        let start = self.samples.len().saturating_sub(count);
        let recent: Vec<&Sample> = self.samples.iter().skip(start).collect();

        let rtts: Vec<f64> = recent
            .iter()
            .filter_map(|s| match s {
                Sample::Reply(d) => Some(d.as_secs_f64()),
                Sample::Timeout => None,
            })
            .collect();

        if rtts.is_empty() {
            return recent.iter().map(|_| None).collect();
        }

        let min = rtts.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = rtts.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;

        recent
            .iter()
            .map(|s| match s {
                Sample::Timeout => None,
                Sample::Reply(d) => {
                    let v = d.as_secs_f64();
                    let level = if range <= f64::EPSILON {
                        // Single-value or flat window: mid-level bar.
                        4u8
                    } else {
                        let normalized = (v - min) / range;
                        (normalized * 7.0).round().clamp(0.0, 7.0) as u8
                    };
                    Some(level)
                }
            })
            .collect()
    }
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
    fn sparkline_single_sample_mid_level() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(15)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(4)]);
    }

    #[test]
    fn sparkline_scales_to_min_max() {
        let mut s = Stats::new(5);
        s.record(Sample::Reply(ms(0)));
        s.record(Sample::Reply(ms(100)));
        let buckets = s.sparkline_buckets(5);
        assert_eq!(buckets, vec![Some(0), Some(7)]);
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
