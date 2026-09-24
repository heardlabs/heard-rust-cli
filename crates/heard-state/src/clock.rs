//! Time, injected.
//!
//! The Python modules read `time.monotonic()` and `time.time()` straight from
//! the module, which is why every test that wants a stale session reaches into
//! `router._sessions["b"].last_event` and backdates it by hand. That works in a
//! language where you can poke a private attribute; it is also why the Python
//! tests cannot express "two events at the same timestamp" without racing.
//!
//! Here the clock is a parameter. Production passes [`SystemClock`], which is
//! the same two calls; the golden corpus passes [`ManualClock`], which makes
//! the whole state layer a pure function of its event log.
//!
//! Both readings are kept because the two Python modules disagree about which
//! they want, on purpose: `agent_state` uses the monotonic clock for durations
//! and idle windows (a wall-clock step would corrupt them) and the wall clock
//! only for the human-readable `last_event_wall`. `multi_agent` and `session`
//! use the wall clock throughout.

use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The two readings the state layer needs, in seconds.
pub trait Clock: Send + Sync {
    /// `time.monotonic()` — never steps, only meaningful as a difference.
    fn monotonic(&self) -> f64;
    /// `time.time()` — seconds since the Unix epoch.
    fn wall(&self) -> f64;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    base: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            base: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        self.base.elapsed().as_secs_f64()
    }

    fn wall(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            // A wall clock set before 1970 is not a reason to crash the daemon.
            .unwrap_or(0.0)
    }
}

/// A clock the caller drives. Monotonic and wall readings are the same number,
/// which is what the corpus wants: one timeline, explicitly advanced.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<f64>,
}

impl ManualClock {
    pub fn new(start: f64) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    pub fn advance(&self, seconds: f64) {
        let mut now = self.now.lock().expect("manual clock poisoned");
        *now += seconds;
    }

    pub fn set(&self, value: f64) {
        let mut now = self.now.lock().expect("manual clock poisoned");
        *now = value;
    }

    pub fn now(&self) -> f64 {
        *self.now.lock().expect("manual clock poisoned")
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> f64 {
        self.now()
    }

    fn wall(&self) -> f64 {
        self.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_only_moves_when_told_to() {
        let clock = ManualClock::new(100.0);
        assert_eq!(clock.monotonic(), 100.0);
        assert_eq!(clock.wall(), 100.0);
        clock.advance(2.5);
        assert_eq!(clock.now(), 102.5);
        clock.set(0.0);
        assert_eq!(clock.now(), 0.0);
    }

    #[test]
    fn system_clock_monotonic_starts_near_zero_and_never_goes_back() {
        let clock = SystemClock::new();
        let first = clock.monotonic();
        let second = clock.monotonic();
        assert!(first >= 0.0);
        assert!(second >= first);
        assert!(clock.wall() > 1_600_000_000.0, "wall clock looks unset");
    }
}
