/// Restart strategy that determines how sibling children are affected when one
/// child exits unexpectedly.
///
/// Modelled after Erlang/OTP supervisor strategies.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Strategy {
    /// Only the exited child is restarted. Other children are unaffected.
    #[default]
    OneForOne,
    /// All children are stopped and restarted when any single child exits
    /// unexpectedly. Use this when children have hard interdependencies and
    /// cannot function correctly without their siblings.
    OneForAll,
}
