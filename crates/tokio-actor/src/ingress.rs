use std::{sync::Arc, time::Instant};

use tokio::sync::{mpsc, watch};

use crate::{
    envelope::Envelope,
    error::{IngressError, SendError},
    observability::{GraphObservability, MessageOperation, SendRejection},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum MailboxError {
    MailboxFull { actor_id: Arc<str> },
    MailboxClosed { actor_id: Arc<str> },
}

impl MailboxError {
    fn closed(actor_id: &Arc<str>) -> Self {
        Self::MailboxClosed {
            actor_id: Arc::clone(actor_id),
        }
    }

    fn from_try_send(actor_id: &Arc<str>, err: mpsc::error::TrySendError<Envelope>) -> Self {
        match err {
            mpsc::error::TrySendError::Full(_) => Self::MailboxFull {
                actor_id: Arc::clone(actor_id),
            },
            mpsc::error::TrySendError::Closed(_) => Self::closed(actor_id),
        }
    }

    pub(crate) fn into_send_error(self) -> SendError {
        match self {
            Self::MailboxFull { actor_id } => SendError::MailboxFull {
                actor_id: actor_id.to_string(),
            },
            Self::MailboxClosed { actor_id } => SendError::MailboxClosed {
                actor_id: actor_id.to_string(),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MailboxRef {
    actor_id: Arc<str>,
    sender: mpsc::Sender<Envelope>,
}

impl MailboxRef {
    pub(crate) fn new(actor_id: Arc<str>, sender: mpsc::Sender<Envelope>) -> Self {
        Self { actor_id, sender }
    }

    pub(crate) fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub(crate) async fn send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.sender
            .send(envelope)
            .await
            .map_err(|_| MailboxError::closed(&self.actor_id))
    }

    pub(crate) fn try_send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.sender
            .try_send(envelope)
            .map_err(|err| MailboxError::from_try_send(&self.actor_id, err))
    }

    pub(crate) fn blocking_send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.try_send(envelope)
    }
}

pub(crate) struct IngressBinding {
    current: watch::Sender<Option<MailboxRef>>,
}

impl Default for IngressBinding {
    fn default() -> Self {
        let (current, _receiver) = watch::channel(None);
        Self { current }
    }
}

impl IngressBinding {
    pub(crate) fn bind(&self, mailbox: MailboxRef) -> bool {
        self.current.send_replace(Some(mailbox)).is_none()
    }

    pub(crate) fn clear(&self) -> bool {
        self.current.send_replace(None).is_some()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<MailboxRef>> {
        self.current.subscribe()
    }
}

/// Stable external sender that targets a named ingress point.
///
/// Handles are bound to the currently-running instance of the graph. If the
/// graph is restarted from the same [`Graph`](crate::Graph), the handle is
/// rebound to the new actor mailbox automatically.
#[derive(Clone)]
pub struct IngressHandle {
    name: Arc<str>,
    actor_id: Arc<str>,
    binding: watch::Receiver<Option<MailboxRef>>,
    observability: GraphObservability,
}

impl IngressHandle {
    pub(crate) fn new(
        name: Arc<str>,
        actor_id: Arc<str>,
        binding: watch::Receiver<Option<MailboxRef>>,
        observability: GraphObservability,
    ) -> Self {
        Self {
            name,
            actor_id,
            binding,
            observability,
        }
    }

    fn current_mailbox(&self) -> Result<MailboxRef, IngressError> {
        self.binding
            .borrow()
            .clone()
            .ok_or_else(|| IngressError::NotRunning {
                ingress: self.name.to_string(),
                actor_id: self.actor_id.to_string(),
            })
    }

    fn map_mailbox_error(&self, err: MailboxError) -> IngressError {
        match err {
            MailboxError::MailboxFull { .. } => IngressError::MailboxFull {
                ingress: self.name.to_string(),
                actor_id: self.actor_id.to_string(),
            },
            MailboxError::MailboxClosed { .. } => IngressError::MailboxClosed {
                ingress: self.name.to_string(),
                actor_id: self.actor_id.to_string(),
            },
        }
    }

    /// Returns the ingress name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the target actor id for the ingress.
    pub fn target(&self) -> &str {
        &self.actor_id
    }

    /// Sends an envelope through the ingress, waiting for mailbox capacity.
    pub async fn send(&self, envelope: impl Into<Envelope>) -> Result<(), IngressError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = Instant::now();

        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .send(envelope)
                .await
                .map_err(|err| self.map_mailbox_error(err)),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::Send,
            envelope_len,
            started_at.elapsed(),
            &result,
        );
        result
    }

    /// Attempts to send an envelope through the ingress without waiting.
    pub fn try_send(&self, envelope: impl Into<Envelope>) -> Result<(), IngressError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = Instant::now();

        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .try_send(envelope)
                .map_err(|err| self.map_mailbox_error(err)),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::TrySend,
            envelope_len,
            started_at.elapsed(),
            &result,
        );
        result
    }

    /// Waits until the ingress is bound to a running graph instance.
    ///
    /// Returns immediately if the graph is already running. Returns if the
    /// graph (and its ingress bindings) is dropped.
    pub async fn wait_for_binding(&mut self) {
        let _ = self.binding.wait_for(|slot| slot.is_some()).await;
    }

    fn observe_send(
        &self,
        operation: MessageOperation,
        envelope_len: usize,
        duration: std::time::Duration,
        result: &Result<(), IngressError>,
    ) {
        match result {
            Ok(()) => self.observability.emit_ingress_message_sent(
                self.name(),
                self.target(),
                operation,
                envelope_len,
                duration,
            ),
            Err(error) => self.observability.emit_ingress_message_rejected(
                self.name(),
                self.target(),
                operation,
                ingress_rejection(error),
                envelope_len,
                duration,
            ),
        }
    }
}

impl std::fmt::Debug for IngressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressHandle")
            .field("name", &self.name)
            .field("target", &self.actor_id)
            .finish()
    }
}

fn ingress_rejection(error: &IngressError) -> SendRejection {
    match error {
        IngressError::NotRunning { .. } => SendRejection::NotRunning,
        IngressError::MailboxFull { .. } => SendRejection::MailboxFull,
        IngressError::MailboxClosed { .. } => SendRejection::MailboxClosed,
    }
}
