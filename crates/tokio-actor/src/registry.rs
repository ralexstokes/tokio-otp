use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::{Arc, OnceLock, RwLock},
};

use thiserror::Error;

use crate::{
    binding::BindingCore, context::ActorRef, error::LookupError, observability::GraphObservability,
};

#[derive(Clone, Debug)]
struct RegistryEntry {
    actor_id: Arc<str>,
    message_type: TypeId,
    message_type_name: &'static str,
    binding: Arc<dyn Any + Send + Sync>,
    observability: Arc<OnceLock<GraphObservability>>,
}

/// Errors returned while registering or deregistering runtime actors.
#[derive(Debug, Error, Clone, Eq, PartialEq)]
pub enum RegistryError {
    /// Actor ids must not be empty.
    #[error("actor id must not be empty")]
    EmptyActorId,
    /// An actor with this id is already registered.
    #[error("duplicate actor id `{0}`")]
    DuplicateActorId(String),
    /// No actor with this id exists in the registry.
    #[error("unknown actor `{0}`")]
    UnknownActorId(String),
}

/// Runtime directory of restart-stable actor references.
///
/// The registry is intended for runtime discovery, not static topology.
/// Static actors should prefer refs captured in their own state at build time.
#[derive(Clone, Debug, Default)]
pub struct ActorRegistry {
    entries: Arc<RwLock<HashMap<Arc<str>, RegistryEntry>>>,
}

impl ActorRegistry {
    /// Creates an empty actor registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the registry has an entry for `actor_id`.
    pub fn contains(&self, actor_id: &str) -> bool {
        self.entries
            .read()
            .expect("actor registry rwlock poisoned")
            .contains_key(actor_id)
    }

    /// Returns the registered actor ids in sorted order.
    pub fn actor_ids(&self) -> Vec<String> {
        let mut actor_ids = self
            .entries
            .read()
            .expect("actor registry rwlock poisoned")
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        actor_ids.sort_unstable();
        actor_ids
    }

    /// Returns a stable typed actor reference for external callers.
    pub fn actor_ref<M: Send + 'static>(&self, actor_id: &str) -> Result<ActorRef<M>, LookupError> {
        self.actor_ref_for_source(actor_id, None)
    }

    /// Removes an actor entry from the registry.
    pub fn deregister(&self, actor_id: &str) -> Result<(), RegistryError> {
        if actor_id.is_empty() {
            return Err(RegistryError::EmptyActorId);
        }

        self.entries
            .write()
            .expect("actor registry rwlock poisoned")
            .remove(actor_id)
            .map(|_| ())
            .ok_or_else(|| RegistryError::UnknownActorId(actor_id.to_owned()))
    }

    pub(crate) fn register_erased(
        &self,
        actor_id: Arc<str>,
        message_type: TypeId,
        message_type_name: &'static str,
        observability: Arc<OnceLock<GraphObservability>>,
        binding: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), RegistryError> {
        if actor_id.is_empty() {
            return Err(RegistryError::EmptyActorId);
        }

        let mut entries = self
            .entries
            .write()
            .expect("actor registry rwlock poisoned");
        if entries.contains_key(actor_id.as_ref()) {
            return Err(RegistryError::DuplicateActorId(actor_id.to_string()));
        }

        entries.insert(
            Arc::clone(&actor_id),
            RegistryEntry {
                actor_id,
                message_type,
                message_type_name,
                binding,
                observability,
            },
        );
        Ok(())
    }

    pub(crate) fn actor_ref_for_source<M: Send + 'static>(
        &self,
        actor_id: &str,
        source_actor_id: Option<&Arc<str>>,
    ) -> Result<ActorRef<M>, LookupError> {
        let entry = self
            .entries
            .read()
            .expect("actor registry rwlock poisoned")
            .get(actor_id)
            .cloned()
            .ok_or_else(|| LookupError::UnknownActor {
                actor_id: actor_id.to_owned(),
            })?;

        if entry.message_type != TypeId::of::<M>() {
            return Err(LookupError::MessageTypeMismatch {
                actor_id: actor_id.to_owned(),
                registered: entry.message_type_name,
                requested: type_name::<M>(),
            });
        }

        let Ok(binding) = entry.binding.downcast::<BindingCore<M>>() else {
            unreachable!("message type id already verified")
        };

        Ok(ActorRef::from_parts(
            entry.actor_id,
            binding.subscribe(),
            entry.observability,
            source_actor_id.cloned(),
        ))
    }
}
