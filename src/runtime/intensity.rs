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
                delay = saturating_mul_duration(delay, factor);
                if delay >= max {
                    return max;
                }
            }
            delay.min(max)
        }
    }
}

fn saturating_mul_duration(duration: Duration, factor: u32) -> Duration {
    let nanos = duration.as_nanos();
    let multiplied = nanos.saturating_mul(u128::from(factor));
    let secs = multiplied / 1_000_000_000;
    let subsec_nanos = (multiplied % 1_000_000_000) as u32;
    if secs > u128::from(u64::MAX) {
        Duration::MAX
    } else {
        Duration::new(secs as u64, subsec_nanos)
    }
}
