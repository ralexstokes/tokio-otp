//! JSON helpers for translating between byte-oriented application edges and
//! typed actor messages.
//!
//! This module is available with the `serde` feature. It deliberately handles
//! only serialization and delivery; framing bytes from a socket, file, or IPC
//! stream remains the responsibility of the application.
//!
//! Use [`codec::decode`](decode) and [`codec::encode`](encode) for pure JSON
//! conversion, or [`codec::decode_and_send`](decode_and_send) to deliver a
//! decoded message directly to an actor.

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{ActorRef, SendError};

/// An error while translating or delivering a message at a byte boundary.
#[derive(Debug, Error)]
pub enum CodecError {
    /// The bytes were not valid JSON for the requested message type.
    #[error("JSON codec error: {0}")]
    Json(#[from] serde_json::Error),
    /// A decoded message could not be delivered to its actor.
    #[error("could not deliver decoded message: {0}")]
    Send(#[from] SendError),
}

/// Decodes one JSON value from bytes into a typed message.
pub fn decode<M>(bytes: impl AsRef<[u8]>) -> Result<M, serde_json::Error>
where
    M: DeserializeOwned,
{
    serde_json::from_slice(bytes.as_ref())
}

/// Encodes a typed message as a JSON byte vector.
pub fn encode<M>(message: &M) -> Result<Vec<u8>, serde_json::Error>
where
    M: Serialize + ?Sized,
{
    serde_json::to_vec(message)
}

/// Decodes one JSON value and sends the typed message to an actor.
///
/// This applies the same backpressure and restart behavior as
/// [`ActorRef::send`]. Framing is intentionally left to the caller.
pub async fn decode_and_send<M>(
    actor: &ActorRef<M>,
    bytes: impl AsRef<[u8]>,
) -> Result<(), CodecError>
where
    M: DeserializeOwned,
{
    actor.send(decode(bytes)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde::{Deserialize, Serialize};

    use super::{CodecError, decode, decode_and_send, encode};
    use crate::{Actor, ActorContext, ActorRef, ActorResult, GraphBuilder, Runtime};

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Message {
        name: String,
        count: u64,
    }

    #[derive(Clone)]
    struct Sink(tokio::sync::mpsc::UnboundedSender<Message>);

    impl Actor for Sink {
        type Msg = Message;

        async fn handle(&mut self, message: Message, _ctx: &ActorContext<Message>) -> ActorResult {
            self.0.send(message).expect("test receiver should be open");
            Ok(())
        }
    }

    #[test]
    fn round_trips_typed_messages() {
        let message = Message {
            name: "widgets".to_owned(),
            count: 3,
        };

        let bytes = encode(&message).expect("message should encode");

        assert_eq!(decode::<Message>(&bytes).unwrap(), message);
    }

    #[test]
    fn reports_invalid_json() {
        assert!(decode::<Message>(b"not json").is_err());
    }

    #[tokio::test]
    async fn reports_invalid_json_when_sending() {
        let actor = ActorRef::<Message>::detached(Arc::from("sink"));
        let error = decode_and_send(&actor, b"not json").await.unwrap_err();

        assert!(matches!(error, CodecError::Json(_)));
    }

    #[tokio::test]
    async fn decodes_and_delivers_typed_messages() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut graph = GraphBuilder::new();
        let sink = graph.add(Sink(tx));
        let handle = Runtime::builder()
            .graph(graph.build().unwrap())
            .build()
            .unwrap()
            .spawn();

        decode_and_send(&sink, br#"{"name":"widgets","count":3}"#)
            .await
            .unwrap();

        assert_eq!(
            rx.recv().await.unwrap(),
            Message {
                name: "widgets".to_owned(),
                count: 3,
            }
        );
        handle.shutdown_and_wait().await.unwrap();
    }

    #[tokio::test]
    async fn reports_actor_delivery_errors() {
        let actor = ActorRef::<Message>::detached(Arc::from("sink"));
        let error = decode_and_send(&actor, br#"{"name":"widgets","count":3}"#)
            .await
            .unwrap_err();

        assert!(matches!(error, CodecError::Send(_)));
    }
}
