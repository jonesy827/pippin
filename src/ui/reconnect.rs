//! When to bring a dropped session back: what a session end means for the
//! window, and how long to wait between attempts.

use std::time::{Duration, Instant};

const FIRST_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Stop reconnecting once this long has passed without any video.
const GIVE_UP_AFTER: Duration = Duration::from_secs(10 * 60);

/// Doubling delay between reconnect attempts, capped at [`MAX_DELAY`].
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { next: FIRST_DELAY }
    }
}

impl Backoff {
    /// Delay before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (d * 2).min(MAX_DELAY);
        d
    }

    /// A session came up; the next drop starts again from the first delay.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the window does when a session ends.
#[derive(Debug, PartialEq, Eq)]
pub enum OnEnd {
    Exit,
    Retry(Duration),
    /// Reconnecting has gone on too long without a picture.
    GiveUp,
}

/// Session lifecycle as the window sees it.
#[derive(Debug, Default)]
pub struct Reconnector {
    enabled: bool,
    /// A session reached `Connected` at least once. Until then failures are
    /// reported and end the program, as without reconnecting.
    connected_once: bool,
    reconnecting: bool,
    backoff: Backoff,
    /// Last sign the host really works: the latest picture, or the first
    /// connect if none has arrived yet.
    alive_at: Option<Instant>,
}

impl Reconnector {
    pub fn new(enabled: bool) -> Self {
        Reconnector { enabled, ..Default::default() }
    }

    pub fn reconnecting(&self) -> bool {
        self.reconnecting
    }

    /// The session finished its handshake. That alone doesn't prove the
    /// host is healthy (it may accept and then send nothing), so the
    /// backoff keeps growing until [`Self::video`].
    pub fn connected(&mut self) {
        self.connected_once = true;
        self.alive_at.get_or_insert_with(Instant::now);
        self.reconnecting = false;
    }

    /// The user restarted the session. Once connected before, the new
    /// session counts as a reconnect, so a failure retries instead of
    /// quitting (if reconnecting is enabled).
    pub fn restarting(&mut self) {
        self.reconnecting = self.connected_once;
    }

    /// A picture arrived: the session really works, so the next drop
    /// starts again from the first delay.
    pub fn video(&mut self) {
        self.video_at(Instant::now());
    }

    fn video_at(&mut self, now: Instant) {
        self.alive_at = Some(now);
        self.backoff.reset();
    }

    /// The session ended; `retryable` is false for failures another attempt
    /// can't fix, such as rejected credentials.
    pub fn ended(&mut self, retryable: bool) -> OnEnd {
        self.ended_at(retryable, Instant::now())
    }

    fn ended_at(&mut self, retryable: bool, now: Instant) -> OnEnd {
        if !(self.enabled && self.connected_once && retryable) {
            return OnEnd::Exit;
        }
        if self.alive_at.is_some_and(|t| now.duration_since(t) >= GIVE_UP_AFTER) {
            return OnEnd::GiveUp;
        }
        self.reconnecting = true;
        OnEnd::Retry(self.backoff.next_delay())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> OnEnd {
        OnEnd::Retry(Duration::from_secs(s))
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::default();
        let got: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), FIRST_DELAY);
    }

    #[test]
    fn initial_failure_exits() {
        let mut r = Reconnector::new(true);
        assert_eq!(r.ended(true), OnEnd::Exit);
        assert!(!r.reconnecting());
    }

    #[test]
    fn restart_counts_as_reconnect_only_after_connecting() {
        let mut r = Reconnector::new(true);
        r.restarting();
        assert!(!r.reconnecting());
        r.connected();
        r.restarting();
        assert!(r.reconnecting());
        assert_eq!(r.ended(true), secs(1));
    }

    #[test]
    fn drop_after_connect_retries_until_connected() {
        let mut r = Reconnector::new(true);
        r.connected();
        assert_eq!(r.ended(true), secs(1));
        assert!(r.reconnecting());
        // Failed attempts keep backing off.
        assert_eq!(r.ended(true), secs(2));
        assert_eq!(r.ended(true), secs(4));
        r.connected();
        r.video();
        assert!(!r.reconnecting());
        assert_eq!(r.ended(true), secs(1));
    }

    #[test]
    fn backoff_resets_only_after_video() {
        let mut r = Reconnector::new(true);
        r.connected();
        r.video();
        assert_eq!(r.ended(true), secs(1));
        // Handshakes that succeed but never deliver video keep backing off.
        r.connected();
        assert_eq!(r.ended(true), secs(2));
        r.connected();
        assert_eq!(r.ended(true), secs(4));
        r.connected();
        r.video();
        assert_eq!(r.ended(true), secs(1));
    }

    #[test]
    fn auth_rejection_exits_even_while_reconnecting() {
        let mut r = Reconnector::new(true);
        r.connected();
        assert_eq!(r.ended(true), secs(1));
        assert_eq!(r.ended(false), OnEnd::Exit);
    }

    #[test]
    fn gives_up_after_ten_minutes_without_video() {
        let mut r = Reconnector::new(true);
        r.connected();
        let t0 = Instant::now();
        r.video_at(t0);
        assert_eq!(r.ended_at(true, t0 + Duration::from_secs(5)), secs(1));
        assert_eq!(r.ended_at(true, t0 + GIVE_UP_AFTER - Duration::from_secs(1)), secs(2));
        assert_eq!(r.ended_at(true, t0 + GIVE_UP_AFTER), OnEnd::GiveUp);
        // A picture in between restarts the clock.
        r.video_at(t0 + GIVE_UP_AFTER);
        assert_eq!(r.ended_at(true, t0 + GIVE_UP_AFTER + Duration::from_secs(60)), secs(1));
    }

    #[test]
    fn disabled_never_retries() {
        let mut r = Reconnector::new(false);
        r.connected();
        assert_eq!(r.ended(true), OnEnd::Exit);
    }
}
