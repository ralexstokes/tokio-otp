use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ChildContext {
    pub id: String,
    pub generation: u64,
    pub token: CancellationToken,
    pub supervisor_token: CancellationToken,
}
