use std::{collections::HashSet, sync::Arc};

use crate::{
    child::ChildSpec,
    error::BuildError,
    restart::RestartIntensity,
    strategy::Strategy,
    supervisor::{Supervisor, SupervisorConfig},
};

/// Builder for constructing a [`Supervisor`] with validated configuration.
///
/// At minimum, one child must be added via [`child`](Self::child) before
/// calling [`build`](Self::build).
///
/// # Example
///
/// ```no_run
/// use tokio_supervisor::{ChildSpec, SupervisorBuilder, Strategy};
///
/// let supervisor = SupervisorBuilder::new()
///     .strategy(Strategy::OneForOne)
///     .child(ChildSpec::new("worker", |ctx| async move {
///         ctx.token.cancelled().await;
///         Ok(())
///     }))
///     .build()
///     .expect("valid config");
/// ```
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
    /// Creates a new builder with default settings: [`OneForOne`](Strategy::OneForOne)
    /// strategy, default [`RestartIntensity`], and no children.
    pub fn new() -> Self {
        Self {
            strategy: Strategy::default(),
            restart_intensity: RestartIntensity::default(),
            children: Vec::new(),
            control_channel_capacity: DEFAULT_CONTROL_CHANNEL_CAPACITY,
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }

    /// Sets the restart strategy. See [`Strategy`] for options.
    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Sets the default restart intensity for all children that do not have a
    /// per-child override.
    #[must_use]
    pub fn restart_intensity(mut self, intensity: RestartIntensity) -> Self {
        self.restart_intensity = intensity;
        self
    }

    /// Appends a child to the supervisor. Children are started in the order
    /// they are added.
    #[must_use]
    pub fn child(mut self, child: ChildSpec) -> Self {
        self.children.push(child);
        self
    }

    /// Sets the bounded capacity of the internal control channel used for
    /// runtime commands (add/remove child). Defaults to 64.
    #[must_use]
    pub fn control_channel_capacity(mut self, capacity: usize) -> Self {
        self.control_channel_capacity = capacity;
        self
    }

    /// Sets the bounded capacity of the event broadcast channel. Slow
    /// subscribers that fall behind this limit will receive a
    /// [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged)
    /// error. Defaults to 256.
    #[must_use]
    pub fn event_channel_capacity(mut self, capacity: usize) -> Self {
        self.event_channel_capacity = capacity;
        self
    }

    /// Validates the configuration and returns a ready-to-run [`Supervisor`].
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] if:
    /// - No children were added.
    /// - Two children share the same id.
    /// - Any channel capacity is zero.
    /// - Any restart intensity or backoff configuration is invalid.
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
