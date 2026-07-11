/// Restart strategy that determines how sibling children are affected when one
/// child exits unexpectedly.
///
/// Modelled after Erlang/OTP supervisor strategies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Strategy {
    /// Only the exited child is restarted. Other children are unaffected.
    #[default]
    OneForOne,
    /// All children are stopped and restarted when any single child exits
    /// unexpectedly. Use this when children have hard interdependencies and
    /// cannot function correctly without their siblings.
    OneForAll,
    /// The exited child and every child declared after it are stopped and
    /// restarted. Children declared before it are unaffected. Use this for
    /// ordered pipelines where downstream state depends on upstream output.
    RestForOne,
}
