use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::{envelope::Envelope, error::SendError, observability::GraphObservability};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum MailboxError {
    EnvelopeTooLarge {
        actor_id: Arc<str>,
        envelope_len: usize,
        max_envelope_bytes: usize,
    },
    MailboxFull {
        actor_id: Arc<str>,
    },
    MailboxClosed {
        actor_id: Arc<str>,
    },
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
            Self::EnvelopeTooLarge {
                actor_id,
                envelope_len,
                max_envelope_bytes,
            } => SendError::EnvelopeTooLarge {
                actor_id: actor_id.to_string(),
                envelope_len,
                max_envelope_bytes,
            },
            Self::MailboxFull { actor_id } => SendError::MailboxFull {
                actor_id: actor_id.to_string(),
            },
            Self::MailboxClosed { actor_id } => SendError::MailboxClosed {
                actor_id: actor_id.to_string(),
            },
        }
    }
}

pub(crate) enum MailboxSendFailure {
    EnvelopeTooLarge {
        envelope_len: usize,
        max_envelope_bytes: usize,
    },
    MailboxClosed {
        envelope: Envelope,
    },
}

impl MailboxSendFailure {
    pub(crate) fn into_mailbox_error(self, actor_id: &Arc<str>) -> MailboxError {
        match self {
            Self::EnvelopeTooLarge {
                envelope_len,
                max_envelope_bytes,
            } => MailboxError::EnvelopeTooLarge {
                actor_id: Arc::clone(actor_id),
                envelope_len,
                max_envelope_bytes,
            },
            Self::MailboxClosed { .. } => MailboxError::MailboxClosed {
                actor_id: Arc::clone(actor_id),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MailboxRef {
    actor_id: Arc<str>,
    sender: mpsc::Sender<Envelope>,
    max_envelope_bytes: Option<usize>,
}

impl MailboxRef {
    pub(crate) fn new(
        actor_id: Arc<str>,
        sender: mpsc::Sender<Envelope>,
        max_envelope_bytes: Option<usize>,
    ) -> Self {
        Self {
            actor_id,
            sender,
            max_envelope_bytes,
        }
    }

    pub(crate) async fn send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.send_retaining(envelope)
            .await
            .map_err(|err| err.into_mailbox_error(&self.actor_id))
    }

    pub(crate) async fn send_retaining(
        &self,
        envelope: Envelope,
    ) -> Result<(), MailboxSendFailure> {
        self.validate_envelope(&envelope).map_err(|err| match err {
            MailboxError::EnvelopeTooLarge {
                envelope_len,
                max_envelope_bytes,
                ..
            } => MailboxSendFailure::EnvelopeTooLarge {
                envelope_len,
                max_envelope_bytes,
            },
            MailboxError::MailboxFull { .. } => {
                unreachable!("async mailbox send never reports full")
            }
            MailboxError::MailboxClosed { .. } => {
                unreachable!("envelope validation never reports a closed mailbox")
            }
        })?;
        self.sender
            .send(envelope)
            .await
            .map_err(|err| MailboxSendFailure::MailboxClosed { envelope: err.0 })
    }

    pub(crate) fn try_send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.validate_envelope(&envelope)?;
        self.sender
            .try_send(envelope)
            .map_err(|err| MailboxError::from_try_send(&self.actor_id, err))
    }

    pub(crate) fn blocking_send(&self, envelope: Envelope) -> Result<(), MailboxError> {
        self.try_send(envelope)
    }

    pub(crate) fn same_channel(&self, other: &Self) -> bool {
        self.sender.same_channel(&other.sender)
    }

    fn validate_envelope(&self, envelope: &Envelope) -> Result<(), MailboxError> {
        let Some(max_envelope_bytes) = self.max_envelope_bytes else {
            return Ok(());
        };

        let envelope_len = envelope.as_slice().len();
        if envelope_len <= max_envelope_bytes {
            return Ok(());
        }

        Err(MailboxError::EnvelopeTooLarge {
            actor_id: Arc::clone(&self.actor_id),
            envelope_len,
            max_envelope_bytes,
        })
    }
}

#[derive(Debug)]
pub(crate) struct MailboxBinding {
    current: watch::Sender<Option<MailboxRef>>,
}

impl Default for MailboxBinding {
    fn default() -> Self {
        let (current, _receiver) = watch::channel(None);
        Self { current }
    }
}

impl MailboxBinding {
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

pub(crate) struct MailboxBindingGuard {
    actor_id: Arc<str>,
    binding: Arc<MailboxBinding>,
    ingress_names: Vec<Arc<str>>,
    observability: GraphObservability,
}

impl MailboxBindingGuard {
    pub(crate) fn bind(
        actor_id: Arc<str>,
        binding: Arc<MailboxBinding>,
        mailbox: MailboxRef,
        ingress_names: Vec<Arc<str>>,
        observability: GraphObservability,
    ) -> Self {
        if binding.bind(mailbox) {
            for ingress in &ingress_names {
                observability.emit_ingress_bound(ingress, &actor_id);
            }
        }

        Self {
            actor_id,
            binding,
            ingress_names,
            observability,
        }
    }
}

impl Drop for MailboxBindingGuard {
    fn drop(&mut self) {
        if self.binding.clear() {
            for ingress in &self.ingress_names {
                self.observability
                    .emit_ingress_cleared(ingress, &self.actor_id);
            }
        }
    }
}
