use std::time::{Duration, Instant};

pub struct FramePacer {
    interval: Duration,
    next: Option<Instant>,
}

impl FramePacer {
    pub fn new(fps: u32) -> Self {
        Self { interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64), next: None }
    }

    pub fn set_fps(&mut self, fps: u32) {
        let old_interval = self.interval;
        self.interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        // Rebase deadline to new cadence: if we had a deadline, recalculate based on when
        // the last frame was emitted and the new interval.
        if let Some(deadline) = self.next {
            if let Some(frame_time) = deadline.checked_sub(old_interval) {
                self.next = Some(frame_time + self.interval);
            }
        }
    }

    pub fn next_deadline(&self) -> Instant {
        self.next.unwrap_or_else(Instant::now)
    }

    pub fn tick(&mut self, now: Instant) -> bool {
        match self.next {
            None => {
                self.next = Some(now + self.interval);
                true
            }
            Some(deadline) if now >= deadline => {
                // Check if we've missed multiple intervals (a stall).
                // If so, snap to cadence from `now` to prevent a burst.
                // Otherwise, advance from `deadline` to maintain regular spacing.
                if now >= deadline + self.interval {
                    self.next = Some(now + self.interval);
                } else {
                    self.next = Some(deadline + self.interval);
                }
                true
            }
            Some(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn emits_at_the_target_rate() {
        let t0 = Instant::now();
        let mut p = FramePacer::new(30); // 33.33 ms
        assert!(p.tick(t0), "first tick emits");
        assert!(!p.tick(t0 + Duration::from_millis(10)), "too soon");
        assert!(p.tick(t0 + Duration::from_millis(34)), "one interval later");
        assert!(!p.tick(t0 + Duration::from_millis(40)));
        assert!(p.tick(t0 + Duration::from_millis(67)));
    }

    #[test]
    fn a_long_stall_does_not_burst() {
        let t0 = Instant::now();
        let mut p = FramePacer::new(60);
        assert!(p.tick(t0));
        // 500 ms gap ~ 30 missed slots; only ONE catch-up frame, then back to cadence.
        assert!(p.tick(t0 + Duration::from_millis(500)));
        assert!(!p.tick(t0 + Duration::from_millis(505)));
        assert!(p.tick(t0 + Duration::from_millis(517)));
    }

    #[test]
    fn set_fps_changes_cadence() {
        let t0 = Instant::now();
        let mut p = FramePacer::new(30);
        assert!(p.tick(t0));
        p.set_fps(60);
        assert!(!p.tick(t0 + Duration::from_millis(10)));
        assert!(p.tick(t0 + Duration::from_millis(17)));
    }
}
