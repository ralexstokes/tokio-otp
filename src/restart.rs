use std::time::Duration;

use crate::error::BuildError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Restart {
    Permanent,
    #[default]
    Transient,
    Temporary,
}

impl Restart {
    pub(crate) fn should_restart(self, is_failure: bool) -> bool {
        match self {
            Self::Permanent => true,
            Self::Transient => is_failure,
            Self::Temporary => false,
        }
    }
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

impl RestartIntensity {
    pub(crate) fn validate(&self) -> Result<(), BuildError> {
        if self.within.is_zero() {
            return Err(BuildError::InvalidConfig(
                "restart intensity window must be non-zero",
            ));
        }

        match self.backoff {
            BackoffPolicy::None => {}
            BackoffPolicy::Fixed(delay) => {
                if delay.is_zero() {
                    return Err(BuildError::InvalidConfig(
                        "fixed backoff delay must be non-zero",
                    ));
                }
            }
            BackoffPolicy::Exponential { base, factor, max } => {
                if base.is_zero() {
                    return Err(BuildError::InvalidConfig(
                        "exponential backoff base must be non-zero",
                    ));
                }
                if factor == 0 {
                    return Err(BuildError::InvalidConfig(
                        "exponential backoff factor must be non-zero",
                    ));
                }
                if max.is_zero() {
                    return Err(BuildError::InvalidConfig(
                        "exponential backoff max must be non-zero",
                    ));
                }
            }
        }

        Ok(())
    }
}
