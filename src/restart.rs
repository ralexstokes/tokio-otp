use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Restart {
    Permanent,
    #[default]
    Transient,
    Temporary,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackoffPolicy {
    #[default]
    None,
    Fixed(Duration),
    Exponential {
        base: Duration,
        factor: u32,
        max: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartIntensity {
    pub max_restarts: usize,
    pub within: Duration,
    pub backoff: BackoffPolicy,
}

impl Default for RestartIntensity {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            within: Duration::from_secs(30),
            backoff: BackoffPolicy::None,
        }
    }
}
