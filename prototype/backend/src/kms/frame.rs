//! When an output may draw: the pacing of frames against page flips.
//!
//! One frame is in flight at a time. A frame is granted when the output is
//! on, the session is active and the previous flip has completed; the next
//! one is granted as soon as that flip completes, if anyone asked. This
//! draws as early as possible, which keeps the logic simple at the cost of
//! up to one refresh period of latency; drawing as late as possible is a
//! refinement for later.
//!
//! [`FramePacer`] is a pure state machine: it is told what happened and
//! answers what to do. Time is passed in, so tests can simulate vblank.

use std::time::Duration;

/// See the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePacer {
    session_active: bool,
    /// When the flip in flight was submitted.
    pending_since: Option<Duration>,
    /// Someone asked for a frame that could not be granted yet.
    requested: bool,
    /// How long a flip may take before it is given up on.
    flip_timeout: Duration,
}

impl FramePacer {
    pub fn new(flip_timeout: Duration) -> Self {
        Self {
            session_active: true,
            pending_since: None,
            requested: false,
            flip_timeout,
        }
    }

    /// Whether a frame can be presented right now.
    pub fn can_present(&self) -> bool {
        self.session_active && self.pending_since.is_none()
    }

    /// Whether a flip is in flight.
    pub fn flip_pending(&self) -> bool {
        self.pending_since.is_some()
    }

    /// Someone wants to draw. Returns `true` when the frame is granted now;
    /// otherwise it is granted when the flip in flight completes or the
    /// session resumes.
    pub fn request_frame(&mut self) -> bool {
        if self.can_present() {
            self.requested = false;
            true
        } else {
            self.requested = true;
            false
        }
    }

    /// Records that a frame is owed without asking for it to be granted
    /// now: it is granted at the next flip completion or session resume.
    /// For a frame that was drawn but could not be shown.
    pub fn owe_frame(&mut self) {
        self.requested = true;
    }

    /// A frame was handed to the device at `now`.
    pub fn flip_submitted(&mut self, now: Duration) {
        self.pending_since = Some(now);
    }

    /// The device reported that the flip completed. Returns whether a flip
    /// was expected at all, and whether a waiting frame request is granted
    /// now.
    pub fn flip_completed(&mut self) -> FlipCompleted {
        let expected = self.pending_since.take().is_some();
        let grant = expected && self.session_active && std::mem::take(&mut self.requested);
        FlipCompleted { expected, grant }
    }

    /// The session was paused. A flip in flight will never report back.
    pub fn session_paused(&mut self) {
        self.session_active = false;
        self.pending_since = None;
    }

    /// The session is active again. Returns whether a waiting frame request
    /// is granted now.
    pub fn session_resumed(&mut self) -> bool {
        self.session_active = true;
        self.pending_since.is_none() && std::mem::take(&mut self.requested)
    }

    /// Whether the flip in flight has been pending for longer than the
    /// timeout at `now`.
    pub fn flip_overdue(&self, now: Duration) -> bool {
        self.pending_since
            .is_some_and(|since| now.saturating_sub(since) > self.flip_timeout)
    }

    /// Gives up on the flip in flight (it is overdue, or the commit turned
    /// out not to have happened). Returns whether a waiting frame request
    /// is granted now.
    pub fn flip_abandoned(&mut self) -> bool {
        self.pending_since = None;
        self.session_active && std::mem::take(&mut self.requested)
    }
}

/// When a frame committed at `now` is expected on screen, at a fixed refresh
/// rate: at the first vblank after `now`, counted from the last completed
/// flip at `last_flip`. A flip still in flight takes that vblank, and the new
/// frame the one after.
///
/// Rendering takes time too: a frame that is late for the vblank found here
/// shows one refresh period later. Without VRR the display does not wait.
pub fn next_vblank(
    last_flip: Duration,
    refresh: Duration,
    now: Duration,
    flip_pending: bool,
) -> Duration {
    let refresh_ns = refresh.as_nanos().max(1);
    let since = now.saturating_sub(last_flip).as_nanos();
    // The first vblank strictly after `now` (and after the last flip).
    let periods = since / refresh_ns + 1;
    let periods = if flip_pending { periods + 1 } else { periods };
    last_flip + Duration::from_nanos((periods * refresh_ns) as u64)
}

/// The answer of [`FramePacer::flip_completed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlipCompleted {
    /// A flip was in flight. `false` for a stray event.
    pub expected: bool,
    /// Grant a frame now.
    pub grant: bool,
}

/// Statistics of the intervals between completed flips.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FlipStatistics {
    pub count: usize,
    pub mean_ms: f64,
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    /// Intervals longer than 1.5 refresh periods: frames that missed a
    /// vblank.
    pub late: usize,
}

/// Summarizes flip timestamps. `refresh` is the nominal refresh period.
pub fn flip_statistics(timestamps: &[Duration], refresh: Duration) -> FlipStatistics {
    let mut intervals: Vec<f64> = timestamps
        .windows(2)
        .map(|pair| pair[1].saturating_sub(pair[0]).as_secs_f64() * 1000.0)
        .collect();
    if intervals.is_empty() {
        return FlipStatistics::default();
    }
    intervals.sort_by(f64::total_cmp);
    let late_threshold = refresh.as_secs_f64() * 1000.0 * 1.5;
    FlipStatistics {
        count: intervals.len(),
        mean_ms: intervals.iter().sum::<f64>() / intervals.len() as f64,
        median_ms: intervals[intervals.len() / 2],
        min_ms: intervals[0],
        max_ms: intervals[intervals.len() - 1],
        late: intervals.iter().filter(|i| **i > late_threshold).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn pacer() -> FramePacer {
        FramePacer::new(Duration::from_secs(1))
    }

    #[test]
    fn an_idle_output_grants_a_frame_at_once() {
        let mut pacer = pacer();
        assert!(pacer.can_present());
        assert!(pacer.request_frame());
        // Nothing was presented, so the next request is granted too.
        assert!(pacer.request_frame());
    }

    #[test]
    fn one_frame_is_in_flight_at_a_time() {
        let mut pacer = pacer();
        assert!(pacer.request_frame());
        pacer.flip_submitted(10 * MS);
        assert!(!pacer.can_present());
        // Asked twice while waiting: one frame at the flip.
        assert!(!pacer.request_frame());
        assert!(!pacer.request_frame());
        assert_eq!(
            pacer.flip_completed(),
            FlipCompleted {
                expected: true,
                grant: true
            }
        );
        // Granted once; the request is consumed.
        pacer.flip_submitted(26 * MS);
        assert_eq!(
            pacer.flip_completed(),
            FlipCompleted {
                expected: true,
                grant: false
            }
        );
        assert!(pacer.can_present());
    }

    #[test]
    fn an_owed_frame_is_granted_at_the_next_opportunity() {
        // Owed while idle: nothing happens until the session comes back.
        let mut pacer = pacer();
        pacer.owe_frame();
        pacer.session_paused();
        assert!(pacer.session_resumed());
        // Owed while a flip is in flight: granted when it completes.
        pacer.flip_submitted(MS);
        pacer.owe_frame();
        assert!(pacer.flip_completed().grant);
        // An ordinary request clears the debt.
        pacer.owe_frame();
        assert!(pacer.request_frame());
        pacer.flip_submitted(MS);
        assert!(!pacer.flip_completed().grant);
    }

    #[test]
    fn stray_flip_events_grant_nothing() {
        let mut pacer = pacer();
        assert_eq!(
            pacer.flip_completed(),
            FlipCompleted {
                expected: false,
                grant: false
            }
        );
    }

    #[test]
    fn a_paused_session_grants_nothing_until_it_resumes() {
        let mut pacer = pacer();
        pacer.flip_submitted(10 * MS);
        pacer.session_paused();
        // The flip in flight is forgotten: it will not report.
        assert!(!pacer.flip_pending());
        assert!(!pacer.can_present());
        assert!(!pacer.request_frame());
        // A late event from before the pause is a stray.
        assert!(!pacer.flip_completed().expected);
        // The waiting request is granted on resume, once.
        assert!(pacer.session_resumed());
        assert!(pacer.can_present());
        pacer.session_paused();
        assert!(!pacer.session_resumed());
    }

    #[test]
    fn overdue_flips_can_be_abandoned() {
        let mut pacer = pacer();
        pacer.flip_submitted(Duration::from_secs(5));
        assert!(!pacer.flip_overdue(Duration::from_millis(5_900)));
        assert!(!pacer.flip_overdue(Duration::from_secs(6)));
        assert!(pacer.flip_overdue(Duration::from_millis(6_001)));
        // A clock that jumped backwards is not "overdue".
        assert!(!pacer.flip_overdue(Duration::from_secs(1)));
        assert!(!pacer.request_frame());
        assert!(pacer.flip_abandoned());
        assert!(pacer.can_present());
        assert!(!pacer.flip_abandoned());
    }

    #[test]
    fn the_next_vblank_follows_the_last_flip() {
        let refresh = Duration::from_micros(16_000);
        let last = Duration::from_secs(10);
        let at = |us: u64| last + Duration::from_micros(us);
        // Right after a flip: the next vblank.
        assert_eq!(next_vblank(last, refresh, at(1_000), false), at(16_000));
        // On a vblank: the one after it.
        assert_eq!(next_vblank(last, refresh, at(16_000), false), at(32_000));
        // Idle for a while: the first vblank after now.
        assert_eq!(next_vblank(last, refresh, at(100_000), false), at(112_000));
        // A flip in flight takes the next vblank.
        assert_eq!(next_vblank(last, refresh, at(1_000), true), at(32_000));
        // A clock behind the flip's timestamp is not a time before it.
        assert_eq!(
            next_vblank(last, refresh, Duration::from_secs(9), false),
            at(16_000)
        );
    }

    #[test]
    fn flip_statistics_summarize_intervals() {
        let refresh = Duration::from_micros(16_667);
        assert_eq!(flip_statistics(&[], refresh), FlipStatistics::default());
        assert_eq!(flip_statistics(&[MS], refresh).count, 0);
        // Four on time, one that skipped a vblank.
        let times: Vec<Duration> = [0u64, 16_667, 33_334, 50_001, 83_335, 100_002]
            .iter()
            .map(|us| Duration::from_micros(*us))
            .collect();
        let stats = flip_statistics(&times, refresh);
        assert_eq!(stats.count, 5);
        assert_eq!(stats.late, 1);
        assert!((stats.min_ms - 16.667).abs() < 1e-6);
        assert!((stats.max_ms - 33.334).abs() < 1e-6);
        assert!((stats.median_ms - 16.667).abs() < 1e-6);
        assert!((stats.mean_ms - 20.0004).abs() < 1e-3);
    }
}
