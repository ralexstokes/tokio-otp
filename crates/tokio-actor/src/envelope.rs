use bytes::Bytes;

/// Opaque message envelope passed between actors.
///
/// This first iteration carries only raw bytes. Internally it uses
/// [`Bytes`] so cloning an envelope is cheap and restart-stable ingress
/// handles can share payloads without extra copies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Envelope(Bytes);

impl Envelope {
    /// Wraps an existing [`Bytes`] payload.
    pub fn new(payload: Bytes) -> Self {
        Self(payload)
    }

    /// Creates an envelope from a static byte slice.
    pub fn from_static(payload: &'static [u8]) -> Self {
        Self(Bytes::from_static(payload))
    }

    /// Returns the payload bytes.
    pub fn payload(&self) -> &Bytes {
        &self.0
    }

    /// Consumes the envelope and returns the owned payload.
    pub fn into_payload(self) -> Bytes {
        self.0
    }

    /// Returns the payload as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<Bytes> for Envelope {
    fn from(value: Bytes) -> Self {
        Self::new(value)
    }
}

impl From<Vec<u8>> for Envelope {
    fn from(value: Vec<u8>) -> Self {
        Self::new(Bytes::from(value))
    }
}

impl From<&'static [u8]> for Envelope {
    fn from(value: &'static [u8]) -> Self {
        Self::from_static(value)
    }
}

impl From<&'static str> for Envelope {
    fn from(value: &'static str) -> Self {
        Self::from_static(value.as_bytes())
    }
}
