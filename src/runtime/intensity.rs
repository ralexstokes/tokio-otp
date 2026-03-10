use std::{
    collections::VecDeque,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::time::Instant;

use crate::restart::{BackoffPolicy, RestartIntensity};

pub(crate) struct RestartTracker {
    intensity: RestartIntensity,
    times: VecDeque<Instant>,
    rng: JitterRng,
    total_restarts: u64,
}

impl RestartTracker {
    pub(crate) fn new(intensity: RestartIntensity) -> Self {
        Self {
            intensity,
            times: VecDeque::new(),
            rng: JitterRng::new(),
            total_restarts: 0,
        }
    }

    pub(crate) fn record(&mut self, now: Instant) {
        while let Some(front) = self.times.front() {
            if now.duration_since(*front) > self.intensity.within {
                self.times.pop_front();
            } else {
                break;
            }
        }
        self.times.push_back(now);
        self.total_restarts = self.total_restarts.saturating_add(1);
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.times.len() > self.intensity.max_restarts
    }

    pub(crate) fn backoff(&mut self) -> Duration {
        let deterministic = match self.intensity.backoff {
            BackoffPolicy::None => return Duration::ZERO,
            BackoffPolicy::Fixed(delay) => return delay,
            BackoffPolicy::Exponential { base, factor, max }
            | BackoffPolicy::JitteredExponential { base, factor, max } => {
                exponential_backoff(base, factor, max, self.times.len().saturating_sub(1))
            }
        };

        match self.intensity.backoff {
            BackoffPolicy::JitteredExponential { .. } => {
                self.rng.jitter_between(deterministic / 2, deterministic)
            }
            BackoffPolicy::None | BackoffPolicy::Fixed(_) | BackoffPolicy::Exponential { .. } => {
                deterministic
            }
        }
    }

    pub(crate) fn total_restarts(&self) -> u64 {
        self.total_restarts
    }
}

fn exponential_backoff(base: Duration, factor: u32, max: Duration, steps: usize) -> Duration {
    let mut delay = base;
    for _ in 0..steps {
        delay = delay.saturating_mul(factor);
        if delay >= max {
            return max;
        }
    }
    delay.min(max)
}

struct JitterRng {
    state: u64,
}

impl JitterRng {
    fn new() -> Self {
        Self {
            state: seed_jitter_rng(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = xorshift64(self.state);
        self.state
    }

    fn jitter_between(&mut self, min: Duration, max: Duration) -> Duration {
        if min >= max {
            return max;
        }

        let min_nanos = min.as_nanos();
        let span = max.as_nanos() - min_nanos;
        let offset = u128::from(self.next_u64()) % (span + 1);
        duration_from_nanos(min_nanos + offset)
    }
}

fn seed_jitter_rng() -> u64 {
    let nanos = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(err) => err.duration().as_nanos(),
    };
    let seed = (nanos as u64) ^ ((nanos >> 64) as u64) ^ 0x9e37_79b9_7f4a_7c15;
    if seed == 0 { 1 } else { seed }
}

fn xorshift64(mut state: u64) -> u64 {
    if state == 0 {
        state = 1;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn duration_from_nanos(nanos: u128) -> Duration {
    let secs = nanos / 1_000_000_000;
    let subsec_nanos = (nanos % 1_000_000_000) as u32;
    Duration::new(secs as u64, subsec_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(policy: BackoffPolicy) -> RestartTracker {
        RestartTracker {
            intensity: RestartIntensity {
                max_restarts: 10,
                within: Duration::from_secs(10),
                backoff: policy,
            },
            times: VecDeque::new(),
            rng: JitterRng {
                state: 0x1234_5678_9abc_def0,
            },
            total_restarts: 0,
        }
    }

    #[test]
    fn exponential_backoff_caps_at_maximum() {
        let delay = exponential_backoff(Duration::from_millis(10), 3, Duration::from_millis(50), 4);

        assert_eq!(delay, Duration::from_millis(50));
    }

    #[test]
    fn jittered_backoff_stays_within_equal_jitter_bounds() {
        let mut tracker = tracker(BackoffPolicy::JitteredExponential {
            base: Duration::from_millis(80),
            factor: 2,
            max: Duration::from_millis(500),
        });
        tracker.record(Instant::now());
        tracker.record(Instant::now());

        let delay = tracker.backoff();

        assert!(delay >= Duration::from_millis(80));
        assert!(delay <= Duration::from_millis(160));
    }
}
