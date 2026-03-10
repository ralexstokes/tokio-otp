use std::{collections::HashSet, sync::Arc};

use crate::{
    child::ChildSpec,
    error::BuildError,
    restart::RestartIntensity,
    strategy::Strategy,
    supervisor::{Supervisor, SupervisorConfig},
};

pub struct SupervisorBuilder {
    strategy: Strategy,
    restart_intensity: RestartIntensity,
    children: Vec<ChildSpec>,
    control_channel_capacity: usize,
    event_channel_capacity: usize,
}

const DEFAULT_CONTROL_CHANNEL_CAPACITY: usize = 64;
const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;

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
            control_channel_capacity: DEFAULT_CONTROL_CHANNEL_CAPACITY,
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }

    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    #[must_use]
    pub fn restart_intensity(mut self, intensity: RestartIntensity) -> Self {
        self.restart_intensity = intensity;
        self
    }

    #[must_use]
    pub fn child(mut self, child: ChildSpec) -> Self {
        self.children.push(child);
        self
    }

    #[must_use]
    pub fn control_channel_capacity(mut self, capacity: usize) -> Self {
        self.control_channel_capacity = capacity;
        self
    }

    #[must_use]
    pub fn event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }

    pub fn build(self) -> Result<Supervisor, BuildError> {
        if self.children.is_empty() {
            return Err(BuildError::EmptyChildren);
        }

        self.restart_intensity.validate()?;
        if self.control_channel_capacity == 0 {
            return Err(BuildError::InvalidConfig(
                "control channel capacity must be non-zero",
            ));
        }
        if self.event_channel_capacity == 0 {
            return Err(BuildError::InvalidConfig(
                "event channel capacity must be non-zero",
            ));
        }

        let mut ids = HashSet::new();
        for child in &self.children {
            if child.id().is_empty() {
                return Err(BuildError::InvalidConfig("child id must not be empty"));
            }
            if let Some(restart_intensity) = child.restart_intensity_override() {
                restart_intensity.validate()?;
            }
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
            control_channel_capacity: self.control_channel_capacity,
            event_channel_capacity: self.event_channel_capacity,
        }))
    }
}
