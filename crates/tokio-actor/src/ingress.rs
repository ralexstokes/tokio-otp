use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    binding::{MailboxError, MailboxRef, MailboxSendFailure},
    envelope::Envelope,
    error::IngressError,
    observability::{GraphObservability, MessageOperation, SendRejection},
};

/// Stable external sender that targets a named ingress point.
///
/// Handles are bound to the currently-running instance of the target actor. If
/// the graph is rerun or the actor is restarted from the same long-lived graph
/// wiring, the handle is rebound to the new mailbox automatically.
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
            MailboxError::EnvelopeTooLarge {
                envelope_len,
                max_envelope_bytes,
                ..
            } => IngressError::EnvelopeTooLarge {
                ingress: self.name.to_string(),
                actor_id: self.actor_id.to_string(),
                envelope_len,
                max_envelope_bytes,
            },
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

    async fn wait_for_next_binding(&mut self) -> bool {
        self.binding.wait_for(|slot| slot.is_some()).await.is_ok()
    }

    async fn wait_for_binding_change_from(&mut self, mailbox: &MailboxRef) -> bool {
        loop {
            let unchanged = self
                .binding
                .borrow_and_update()
                .as_ref()
                .is_some_and(|current| current.same_channel(mailbox));
            if !unchanged {
                return true;
            }
            if self.binding.changed().await.is_err() {
                return false;
            }
        }
    }

    fn observe_send(
        &self,
        operation: MessageOperation,
        envelope_len: usize,
        duration: std::time::Duration,
        result: &Result<(), IngressError>,
    ) {
        self.observability.emit_ingress_message(
            &self.name,
            &self.actor_id,
            operation,
            envelope_len,
            duration,
            result.as_ref().err().map(ingress_rejection),
        );
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
        let started_at = self.observability.start_message_timing();

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
            GraphObservability::finish_message_timing(started_at),
            &result,
        );
        result
    }

    /// Retries a send across transient restart windows until the ingress is
    /// rebound or the binding source is dropped.
    pub async fn send_when_ready(
        &mut self,
        envelope: impl Into<Envelope>,
    ) -> Result<(), IngressError> {
        let mut envelope = envelope.into();

        loop {
            let envelope_len = envelope.as_slice().len();
            let started_at = self.observability.start_message_timing();

            match self.current_mailbox() {
                Ok(mailbox) => match mailbox.send_retaining(envelope).await {
                    Ok(()) => {
                        let result = Ok(());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        return result;
                    }
                    Err(MailboxSendFailure::EnvelopeTooLarge {
                        envelope_len,
                        max_envelope_bytes,
                        ..
                    }) => {
                        let error = IngressError::EnvelopeTooLarge {
                            ingress: self.name.to_string(),
                            actor_id: self.actor_id.to_string(),
                            envelope_len,
                            max_envelope_bytes,
                        };
                        let result = Err(error.clone());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        return Err(error);
                    }
                    Err(MailboxSendFailure::MailboxClosed {
                        envelope: returned, ..
                    }) => {
                        let error = IngressError::MailboxClosed {
                            ingress: self.name.to_string(),
                            actor_id: self.actor_id.to_string(),
                        };
                        let result = Err(error.clone());
                        self.observe_send(
                            MessageOperation::Send,
                            envelope_len,
                            GraphObservability::finish_message_timing(started_at),
                            &result,
                        );
                        envelope = returned;
                        if !self.wait_for_binding_change_from(&mailbox).await {
                            return Err(error);
                        }
                    }
                },
                Err(error) => {
                    let result = Err(error.clone());
                    self.observe_send(
                        MessageOperation::Send,
                        envelope_len,
                        GraphObservability::finish_message_timing(started_at),
                        &result,
                    );
                    if !self.wait_for_next_binding().await {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Attempts to send an envelope through the ingress without waiting.
    pub fn try_send(&self, envelope: impl Into<Envelope>) -> Result<(), IngressError> {
        let envelope = envelope.into();
        let envelope_len = envelope.as_slice().len();
        let started_at = self.observability.start_message_timing();

        let result = match self.current_mailbox() {
            Ok(mailbox) => mailbox
                .try_send(envelope)
                .map_err(|err| self.map_mailbox_error(err)),
            Err(error) => Err(error),
        };
        self.observe_send(
            MessageOperation::TrySend,
            envelope_len,
            GraphObservability::finish_message_timing(started_at),
            &result,
        );
        result
    }

    /// Waits until the ingress is bound to a running actor instance.
    ///
    /// Returns immediately if the target actor is already running. Returns if
    /// the binding source is dropped.
    pub async fn wait_for_binding(&mut self) {
        let _ = self.wait_for_next_binding().await;
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
        IngressError::EnvelopeTooLarge { .. } => SendRejection::EnvelopeTooLarge,
        IngressError::MailboxFull { .. } => SendRejection::MailboxFull,
        IngressError::MailboxClosed { .. } => SendRejection::MailboxClosed,
    }
}
