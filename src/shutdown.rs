use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownMode {
    Cooperative,
    CooperativeThenAbort,
    Abort,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownPolicy {
    pub grace: Duration,
    pub mode: ShutdownMode,
}

impl Default for ShutdownPolicy {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(5),
            mode: ShutdownMode::CooperativeThenAbort,
        }
    }
}
