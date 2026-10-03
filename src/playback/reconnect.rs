//! M7 §7: reconnect timing, as pure data. No I/O, no clock of its own — the
//! worker passes `Instant`s and listening time in, so every rule here is an
//! assertion rather than a sleep.

use std::time::{Duration, Instant};

use crate::http::error::RemoteFailure;
use crate::media::capabilities::Continuity;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconnectPolicy {
    /// Delay before attempt n; the last entry repeats.
    pub backoff: [Duration; 5],
    /// Wall time from the outage's first failure, evaluated only when
    /// something fails. It never cuts an in-flight open short and never stops
    /// playback that is succeeding.
    pub budget: Duration,
    /// Heard audio after a reconnect that ends the outage. Played audio
    /// only: bytes, decoded frames and seeks do not count.
    pub stable_after: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            backoff: [1, 2, 4, 8, 15].map(Duration::from_secs),
            budget: Duration::from_secs(5 * 60),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    AttemptAt(Instant),
    GiveUp,
}

/// M10 §3: whether trying the same location again can help, for media of
/// this continuity. A station's body has no declared length, so it is never
/// "truncated", and only a station can end live. A finite body that ended
/// early can be fetched again from where it broke.
pub fn retryable(failure: &RemoteFailure, continuity: Continuity) -> bool {
    match failure {
        RemoteFailure::LiveEnded => continuity == Continuity::Indefinite,
        RemoteFailure::TruncatedBody { .. } => continuity == Continuity::Finite,
        other => other.is_retryable(),
    }
}

#[derive(Clone, Debug)]
pub struct Outage {
    started: Instant,
    failures: usize,
    next_attempt_at: Instant,
    /// Whether a reconnect is playing and its stability window has started.
    window_open: bool,
    /// Audio heard since the window opened (M10 §6). Seeks move the position
    /// but not this, so they neither end nor extend the window.
    heard: Duration,
}

impl Outage {
    pub fn begin(now: Instant) -> Self {
        Self {
            started: now,
            failures: 0,
            next_attempt_at: now,
            window_open: false,
            heard: Duration::ZERO,
        }
    }

    /// A playing connection or an attempt failed.
    pub fn failed(&mut self, now: Instant, policy: &ReconnectPolicy) -> Next {
        self.window_open = false;
        self.heard = Duration::ZERO;
        if now.duration_since(self.started) >= policy.budget {
            return Next::GiveUp;
        }
        let step = self.failures.min(policy.backoff.len() - 1);
        self.failures += 1;
        self.next_attempt_at = now + policy.backoff[step];
        Next::AttemptAt(self.next_attempt_at)
    }

    pub fn due(&self, now: Instant) -> bool {
        !self.window_open && now >= self.next_attempt_at
    }

    /// A reconnect started playing: the stability window opens at zero.
    pub fn playing_from(&mut self) {
        self.window_open = true;
        self.heard = Duration::ZERO;
    }

    /// Credit audio heard since the last reading. Ignored until a reconnect
    /// is playing, so ring drain during backoff never counts.
    pub fn add_heard(&mut self, heard: Duration) {
        if self.window_open {
            self.heard = self.heard.saturating_add(heard);
        }
    }

    pub fn is_over(&self, policy: &ReconnectPolicy) -> bool {
        self.window_open && self.heard >= policy.stable_after
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    #[test]
    fn backoff_steps_then_repeats_its_last_entry() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        let delays: Vec<u64> = (0..7)
            .map(|_| match outage.failed(t0, &policy()) {
                Next::AttemptAt(at) => at.duration_since(t0).as_secs(),
                Next::GiveUp => panic!("inside the budget"),
            })
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn the_budget_is_judged_only_when_something_fails() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        assert!(matches!(outage.failed(t0, &policy()), Next::AttemptAt(_)));
        let late = t0 + Duration::from_secs(301);
        assert!(!outage.is_over(&policy()), "time alone ends nothing");
        assert_eq!(outage.failed(late, &policy()), Next::GiveUp);
    }

    #[test]
    fn an_attempt_is_due_at_its_scheduled_instant_and_not_a_moment_before() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        assert!(!outage.due(t0 + Duration::from_millis(999)));
        assert!(outage.due(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn short_connections_stay_one_outage_and_thirty_heard_seconds_end_it() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from();
        assert!(
            !outage.due(t0 + Duration::from_secs(60)),
            "no attempt while playing"
        );
        outage.add_heard(Duration::from_secs(2));
        assert!(!outage.is_over(&policy()));
        // It closed after two seconds: same outage, next backoff step.
        assert_eq!(
            outage.failed(t0 + Duration::from_secs(3), &policy()),
            Next::AttemptAt(t0 + Duration::from_secs(5))
        );
        outage.playing_from();
        outage.add_heard(Duration::from_secs(29));
        assert!(!outage.is_over(&policy()));
        outage.add_heard(Duration::from_secs(1));
        assert!(outage.is_over(&policy()));
    }

    #[test]
    fn heard_time_counts_only_once_a_reconnect_is_playing() {
        // M10 §6: the ring drains during backoff, and that audio is heard,
        // but no reconnect has started playing yet, so it is not stability.
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.add_heard(Duration::from_secs(60));
        assert!(!outage.is_over(&policy()));
        outage.playing_from();
        assert!(!outage.is_over(&policy()), "the window starts from zero");
    }

    #[test]
    fn a_failure_closes_the_window_and_forgets_what_was_heard() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from();
        outage.add_heard(Duration::from_secs(29));
        outage.failed(t0 + Duration::from_secs(30), &policy());
        outage.add_heard(Duration::from_secs(5));
        outage.playing_from();
        outage.add_heard(Duration::from_secs(1));
        assert!(!outage.is_over(&policy()));
    }

    #[test]
    fn retry_classification_follows_continuity() {
        use crate::http::error::{Operation, Phase};
        let truncated = RemoteFailure::TruncatedBody { missing: 1 };
        for continuity in [Continuity::Indefinite, Continuity::Finite] {
            let transport = RemoteFailure::Transport {
                operation: Operation::Read,
                detail: String::new(),
            };
            assert!(retryable(&transport, continuity));
            assert!(retryable(
                &RemoteFailure::Timeout {
                    phase: Phase::Stall
                },
                continuity
            ));
            assert!(retryable(
                &RemoteFailure::Status {
                    status: 503,
                    operation: Operation::Open
                },
                continuity
            ));
            assert!(!retryable(&RemoteFailure::ResourceChanged, continuity));
            assert!(!retryable(
                &RemoteFailure::Status {
                    status: 404,
                    operation: Operation::Open
                },
                continuity
            ));
        }
        assert!(retryable(&RemoteFailure::LiveEnded, Continuity::Indefinite));
        assert!(!retryable(&truncated, Continuity::Indefinite));
        assert!(retryable(&truncated, Continuity::Finite));
    }
}
