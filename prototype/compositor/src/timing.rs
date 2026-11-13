//! How long frames take to make: measured, summarized, reported.
//!
//! "Draw" is the scene's work up to the point where the frame's image is
//! complete (uploads, the render pass, and waiting for the GPU where the
//! renderer waits). "Present" is the backend's part: handing the image to
//! the display, which on the CPU copy path includes reading the frame back
//! and copying it into the scanout buffer.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::{Value, json};

/// How many recent frames are kept.
const WINDOW: usize = 1024;

/// The times of the most recent frames.
#[derive(Debug, Default)]
pub struct FrameTimes {
    /// `(draw, present)` in milliseconds.
    samples: VecDeque<(f64, f64)>,
    total_frames: u64,
}

impl FrameTimes {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, draw: Duration, present: Duration) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples
            .push_back((draw.as_secs_f64() * 1000.0, present.as_secs_f64() * 1000.0));
        self.total_frames += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The median of draw plus present, in milliseconds.
    pub fn median_total_ms(&self) -> Option<f64> {
        let mut totals: Vec<f64> = self.samples.iter().map(|(d, p)| d + p).collect();
        if totals.is_empty() {
            return None;
        }
        totals.sort_by(f64::total_cmp);
        Some(totals[totals.len() / 2])
    }

    /// Statistics over the kept frames, in milliseconds.
    pub fn describe(&self) -> Value {
        let column = |pick: fn(&(f64, f64)) -> f64| {
            summarize(self.samples.iter().map(pick).collect::<Vec<_>>())
        };
        json!({
            "frames": self.total_frames,
            "window": self.samples.len(),
            "draw": column(|s| s.0),
            "present": column(|s| s.1),
            "total": column(|s| s.0 + s.1),
        })
    }
}

fn summarize(mut values: Vec<f64>) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(f64::total_cmp);
    let at = |fraction: f64| values[((values.len() - 1) as f64 * fraction).round() as usize];
    json!({
        "mean": values.iter().sum::<f64>() / values.len() as f64,
        "median": at(0.5),
        "p99": at(0.99),
        "min": values[0],
        "max": values[values.len() - 1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn nothing_recorded_is_reported_as_nothing() {
        let times = FrameTimes::new();
        assert!(times.is_empty());
        assert_eq!(times.median_total_ms(), None);
        let described = times.describe();
        assert_eq!(described["frames"], 0);
        assert!(described["total"].is_null());
    }

    #[test]
    fn frames_are_summarized() {
        let mut times = FrameTimes::new();
        for i in 1..=101u32 {
            times.record(MS * i, MS * 2);
        }
        let described = times.describe();
        assert_eq!(described["frames"], 101);
        assert_eq!(described["draw"]["min"], 1.0);
        assert_eq!(described["draw"]["median"], 51.0);
        assert_eq!(described["draw"]["p99"], 100.0);
        assert_eq!(described["draw"]["max"], 101.0);
        assert_eq!(described["present"]["mean"], 2.0);
        assert_eq!(described["total"]["median"], 53.0);
        assert_eq!(times.median_total_ms(), Some(53.0));
    }

    #[test]
    fn only_the_recent_frames_are_kept() {
        let mut times = FrameTimes::new();
        for _ in 0..WINDOW {
            times.record(MS * 100, MS);
        }
        for _ in 0..WINDOW {
            times.record(MS, MS);
        }
        let described = times.describe();
        assert_eq!(described["frames"], 2 * WINDOW as u64);
        assert_eq!(described["window"], WINDOW as u64);
        assert_eq!(described["draw"]["max"], 1.0);
    }
}
