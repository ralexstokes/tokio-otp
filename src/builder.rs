use std::{collections::HashSet, sync::Arc};

use crate::{
    child::ChildSpec,
    error::BuildError,
    restart::{BackoffPolicy, RestartIntensity},
    strategy::Strategy,
    supervisor::{Supervisor, SupervisorConfig},
};

pub struct SupervisorBuilder {
    strategy: Strategy,
    restart_intensity: RestartIntensity,
    children: Vec<ChildSpec>,
}

impl Default for SupervisorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SupervisorBuilder {
    pub fn new() -> Self {
        Self {
            strategy: Strategy::default(),
            restart_intensity: RestartIntensity::default(),
            children: Vec::new(),
        }
    }

    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    pub fn restart_intensity(mut self, intensity: RestartIntensity) -> Self {
        self.restart_intensity = intensity;
        self
    }

    pub fn child(mut self, child: ChildSpec) -> Self {
        self.children.push(child);
        self
    }

    pub fn build(self) -> Result<Supervisor, BuildError> {
        if self.children.is_empty() {
            return Err(BuildError::EmptyChildren);
        }

        validate_restart_intensity(&self.restart_intensity)?;

        let mut ids = HashSet::new();
        for child in &self.children {
            if !ids.insert(child.id()) {
                return Err(BuildError::DuplicateChildId(child.id().to_owned()));
            }
        }

        Ok(Supervisor::new(SupervisorConfig {
            strategy: self.strategy,
            restart_intensity: self.restart_intensity,
            children: self
                .children
                .into_iter()
                .map(|child| Arc::clone(&child.inner))
                .collect(),
        }))
    }
}

fn validate_restart_intensity(intensity: &RestartIntensity) -> Result<(), BuildError> {
    if intensity.within.is_zero() {
        return Err(BuildError::InvalidConfig(
            "restart intensity window must be non-zero",
        ));
    }

    match intensity.backoff {
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
