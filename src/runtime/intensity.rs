use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

use crate::restart::{BackoffPolicy, RestartIntensity};

pub(crate) fn prune_restart_window(
    restart_times: &mut VecDeque<Instant>,
    within: Duration,
    now: Instant,
) {
    while let Some(front) = restart_times.front() {
        if now.duration_since(*front) > within {
            restart_times.pop_front();
        } else {
            break;
        }
    }
}

pub(crate) fn record_restart(restart_times: &mut VecDeque<Instant>, now: Instant) {
    restart_times.push_back(now);
}

pub(crate) fn intensity_exceeded(restart_times: &VecDeque<Instant>, max_restarts: usize) -> bool {
    restart_times.len() > max_restarts
}

pub(crate) fn compute_backoff(
    intensity: &RestartIntensity,
    restart_count_in_window: usize,
) -> Duration {
    match intensity.backoff {
        BackoffPolicy::None => Duration::ZERO,
        BackoffPolicy::Fixed(delay) => delay,
        BackoffPolicy::Exponential { base, factor, max } => {
            let mut delay = base;
            let steps = restart_count_in_window.saturating_sub(1);
            for _ in 0..steps {
                delay = delay.saturating_mul(factor);
                if delay >= max {
                    return max;
                }
            }
            delay.min(max)
        }
    }
}
