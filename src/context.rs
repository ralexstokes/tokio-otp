use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ChildContext {
    pub id: Arc<str>,
    pub generation: u64,
    pub token: CancellationToken,
    pub supervisor_token: CancellationToken,
}
