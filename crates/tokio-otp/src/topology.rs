/// A descriptive snapshot of a derived actor topology.
///
/// [`Topology`](crate::Topology) generates this value only when the topology
/// opts in with `#[topology(metadata)]`. It describes actor types and declared
/// message-flow edges; it does not affect graph construction or execution.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TopologyMetadata {
    /// Actors in declaration order.
    pub nodes: Vec<TopologyNode>,
    /// Message-flow edges in declaration order.
    pub edges: Vec<TopologyEdge>,
}

/// One actor in a [`TopologyMetadata`] description.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TopologyNode {
    /// Actor label, equal to the topology field name.
    pub name: String,
    /// Fully qualified Rust actor type name.
    ///
    /// This value comes from [`std::any::type_name`], whose exact output is not
    /// guaranteed stable across compiler versions and must not be treated as a
    /// stable identifier.
    pub actor_type: String,
    /// Fully qualified Rust message type name accepted by the actor.
    ///
    /// This value comes from [`std::any::type_name`], whose exact output is not
    /// guaranteed stable across compiler versions and must not be treated as a
    /// stable identifier.
    pub message_type: String,
}

/// A declared message-flow edge in a [`TopologyMetadata`] description.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TopologyEdge {
    /// Actor field that sends the message.
    pub source: String,
    /// Actor field that receives the message.
    pub target: String,
    /// Fully qualified message type accepted by the target actor.
    ///
    /// This value comes from [`std::any::type_name`], whose exact output is not
    /// guaranteed stable across compiler versions and must not be treated as a
    /// stable identifier.
    pub message_type: String,
}
