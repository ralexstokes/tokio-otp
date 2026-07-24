//! Deterministic, in-process simulation of a redelivering chat transport.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::messages::{ChatDelivery, ChatId, EnvelopeId};

#[derive(Clone, Debug)]
pub enum ChatEvent {
    Delivery(ChatDelivery),
    Disconnected,
}

#[derive(Debug, Default)]
struct ChatState {
    next_envelope: EnvelopeId,
    pending: VecDeque<ChatDelivery>,
    live: Option<mpsc::UnboundedSender<ChatEvent>>,
    sessions: usize,
    presentations: usize,
    acks: usize,
    replies: HashMap<ChatId, Vec<String>>,
    progress: HashMap<ChatId, Vec<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct ChatSim(Arc<Mutex<ChatState>>);

pub struct ChatSession {
    receiver: mpsc::UnboundedReceiver<ChatEvent>,
}

impl ChatSession {
    pub async fn recv(&mut self) -> Option<ChatEvent> {
        self.receiver.recv().await
    }
}

impl ChatSim {
    pub fn connect(&self) -> ChatSession {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = self.0.lock().expect("chat simulation lock poisoned");
        assert!(state.live.is_none(), "only one ChatSim session may be live");
        state.sessions += 1;
        state.live = Some(sender.clone());
        let pending = state.pending.iter().cloned().collect::<Vec<_>>();
        // Present each outstanding envelope twice on reconnect. The transport
        // is allowed to redeliver until it observes an ack, and this makes the
        // journal's duplicate path deterministic rather than timing-based.
        for delivery in pending {
            for _ in 0..2 {
                sender
                    .send(ChatEvent::Delivery(delivery.clone()))
                    .expect("new chat session receiver is live");
                state.presentations += 1;
            }
        }
        ChatSession { receiver }
    }

    pub fn inject_user_message(&self, chat: ChatId, text: impl Into<String>) -> EnvelopeId {
        let mut state = self.0.lock().expect("chat simulation lock poisoned");
        state.next_envelope += 1;
        let delivery = ChatDelivery {
            envelope: state.next_envelope,
            chat,
            text: text.into(),
        };
        state.pending.push_back(delivery.clone());
        if let Some(sender) = state.live.clone() {
            sender
                .send(ChatEvent::Delivery(delivery))
                .expect("live chat session receiver");
            state.presentations += 1;
        }
        state.next_envelope
    }

    pub fn ack(&self, envelope: EnvelopeId) {
        let mut state = self.0.lock().expect("chat simulation lock poisoned");
        let before = state.pending.len();
        state.pending.retain(|item| item.envelope != envelope);
        if state.pending.len() != before {
            state.acks += 1;
        }
    }

    pub fn drop_session(&self) {
        let sender = self
            .0
            .lock()
            .expect("chat simulation lock poisoned")
            .live
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(ChatEvent::Disconnected);
        }
    }

    pub fn release_session(&self) {
        self.0.lock().expect("chat simulation lock poisoned").live = None;
    }

    pub fn deliver_reply(&self, chat: ChatId, text: String) {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .replies
            .entry(chat)
            .or_default()
            .push(text);
    }

    pub fn update_progress(&self, chat: ChatId, line: String) {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .progress
            .entry(chat)
            .or_default()
            .push(line);
    }

    pub fn sessions(&self) -> usize {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .sessions
    }

    pub fn presentations(&self) -> usize {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .presentations
    }

    pub fn acks(&self) -> usize {
        self.0.lock().expect("chat simulation lock poisoned").acks
    }

    pub fn replies(&self, chat: ChatId) -> Vec<String> {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .replies
            .get(chat)
            .cloned()
            .unwrap_or_default()
    }

    pub fn progress_count(&self, chat: ChatId) -> usize {
        self.0
            .lock()
            .expect("chat simulation lock poisoned")
            .progress
            .get(chat)
            .map_or(0, Vec::len)
    }
}
