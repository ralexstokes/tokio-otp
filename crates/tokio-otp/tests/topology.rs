use std::time::Duration;

use tokio::{sync::mpsc, task::JoinHandle};
use tokio_otp::{
    Actor, ActorContext, ActorRef, ActorResult, ActorRunError, Graph, GraphBuildError,
    GraphBuilder, RawActor, RebindPolicy, SendError, Topology, TopologyEdge, TopologyMetadata,
    TopologyNode,
};
use tokio_util::sync::CancellationToken;

fn start_graph(
    graph: &Graph,
) -> (
    CancellationToken,
    Vec<JoinHandle<Result<(), ActorRunError>>>,
) {
    let stop = CancellationToken::new();
    let tasks = graph
        .actors()
        .iter()
        .cloned()
        .map(|actor| {
            let stop = stop.clone();
            tokio::spawn(
                async move { actor.run_until(stop.cancelled(), RebindPolicy::Never).await },
            )
        })
        .collect();
    (stop, tasks)
}

async fn stop_graph(stop: CancellationToken, tasks: Vec<JoinHandle<Result<(), ActorRunError>>>) {
    stop.cancel();
    for task in tasks {
        task.await
            .expect("actor task joined")
            .expect("actor stopped cleanly");
    }
}

enum FrontendMsg {
    Feed(String),
    Ack,
}

struct ParserMsg(String);

struct SinkMsg(String);

#[derive(Clone)]
struct Frontend {
    parser: ActorRef<ParserMsg>,
    acks: mpsc::UnboundedSender<()>,
}

impl Actor for Frontend {
    type Msg = FrontendMsg;

    async fn handle(
        &mut self,
        message: FrontendMsg,
        _ctx: &ActorContext<FrontendMsg>,
    ) -> ActorResult {
        match message {
            FrontendMsg::Feed(line) => self.parser.send(ParserMsg(line)).await?,
            FrontendMsg::Ack => self.acks.send(()).expect("test receiver alive"),
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Parser {
    frontend: ActorRef<FrontendMsg>,
    sink: ActorRef<SinkMsg>,
}

impl Actor for Parser {
    type Msg = ParserMsg;

    async fn handle(&mut self, message: ParserMsg, _ctx: &ActorContext<ParserMsg>) -> ActorResult {
        self.sink.send(SinkMsg(message.0.to_uppercase())).await?;
        self.frontend.send(FrontendMsg::Ack).await?;
        Ok(())
    }
}

#[derive(Clone)]
struct Sink {
    out: mpsc::UnboundedSender<String>,
}

impl Actor for Sink {
    type Msg = SinkMsg;

    async fn handle(&mut self, message: SinkMsg, _ctx: &ActorContext<SinkMsg>) -> ActorResult {
        self.out.send(message.0).expect("test receiver alive");
        Ok(())
    }
}

#[derive(Topology)]
#[topology(metadata)]
struct Pipeline {
    #[topology(sends_to(parser))]
    frontend: Frontend,
    #[topology(sends_to(frontend, sink))]
    parser: Parser,
    sink: Sink,
}

#[test]
fn derived_topology_describes_nodes_and_edges() {
    let metadata = Pipeline::topology_metadata();

    assert_eq!(
        metadata,
        TopologyMetadata {
            nodes: vec![
                TopologyNode {
                    name: "frontend".to_owned(),
                    actor_type: std::any::type_name::<Frontend>().to_owned(),
                    message_type: std::any::type_name::<FrontendMsg>().to_owned(),
                },
                TopologyNode {
                    name: "parser".to_owned(),
                    actor_type: std::any::type_name::<Parser>().to_owned(),
                    message_type: std::any::type_name::<ParserMsg>().to_owned(),
                },
                TopologyNode {
                    name: "sink".to_owned(),
                    actor_type: std::any::type_name::<Sink>().to_owned(),
                    message_type: std::any::type_name::<SinkMsg>().to_owned(),
                },
            ],
            edges: vec![
                TopologyEdge {
                    source: "frontend".to_owned(),
                    target: "parser".to_owned(),
                    message_type: std::any::type_name::<ParserMsg>().to_owned(),
                },
                TopologyEdge {
                    source: "parser".to_owned(),
                    target: "frontend".to_owned(),
                    message_type: std::any::type_name::<FrontendMsg>().to_owned(),
                },
                TopologyEdge {
                    source: "parser".to_owned(),
                    target: "sink".to_owned(),
                    message_type: std::any::type_name::<SinkMsg>().to_owned(),
                },
            ],
        }
    );
}

#[cfg(feature = "serde")]
#[test]
fn topology_metadata_serializes() {
    let value = serde_json::to_value(Pipeline::topology_metadata()).expect("serialize metadata");

    assert_eq!(value["nodes"][0]["name"], "frontend");
    assert_eq!(value["edges"][2]["target"], "sink");
}

#[tokio::test]
async fn derived_topology_runs_cyclic_pipeline() {
    let (acks_tx, mut acks_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();

    let mut frontend = None;
    let graph = Pipeline::graph(|refs| {
        frontend = Some(refs.frontend.clone());
        Pipeline {
            frontend: Frontend {
                parser: refs.parser.clone(),
                acks: acks_tx.clone(),
            },
            parser: Parser {
                frontend: refs.frontend.clone(),
                sink: refs.sink.clone(),
            },
            sink: Sink {
                out: out_tx.clone(),
            },
        }
    })
    .expect("valid graph");

    let frontend = frontend.expect("topology closure captured frontend ref");
    let (stop, tasks) = start_graph(&graph);

    frontend
        .send(FrontendMsg::Feed("hello".to_owned()))
        .await
        .expect("send feed");

    assert_eq!(out_rx.recv().await.as_deref(), Some("HELLO"));
    assert_eq!(acks_rx.recv().await, Some(()));

    stop_graph(stop, tasks).await;
}

#[tokio::test]
async fn add_names_actor_after_its_type() {
    let (out_tx, _out_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let first = builder.add(Sink {
        out: out_tx.clone(),
    });
    let second = builder.add(Sink { out: out_tx });

    assert_eq!(first.id(), "Sink");
    assert_eq!(second.id(), "Sink-2");

    builder.build().expect("valid graph");
}

#[test]
fn unfilled_slot_is_a_build_error() {
    let (out_tx, _out_rx) = mpsc::unbounded_channel();

    let mut builder = GraphBuilder::new();
    let (_slot, _sink_ref) = builder.slot::<SinkMsg>("sink");
    builder.add(Sink { out: out_tx });

    match builder.build() {
        Err(GraphBuildError::MissingActor { actor_id }) => assert_eq!(actor_id, "sink"),
        Ok(_) => panic!("expected MissingActor, got valid graph"),
        Err(error) => panic!("expected MissingActor, got {error:?}"),
    }
}

#[test]
fn duplicate_slot_name_is_a_build_error() {
    let mut builder = GraphBuilder::new();
    let (_a, _) = builder.slot::<SinkMsg>("sink");
    let (_b, _) = builder.slot::<SinkMsg>("sink");

    match builder.build() {
        Err(GraphBuildError::DuplicateActorId { actor_id }) => assert_eq!(actor_id, "sink"),
        Ok(_) => panic!("expected DuplicateActorId, got valid graph"),
        Err(error) => panic!("expected DuplicateActorId, got {error:?}"),
    }
}

#[test]
fn slot_token_from_another_builder_is_a_build_error() {
    let (out_tx, _out_rx) = mpsc::unbounded_channel();

    let mut other = GraphBuilder::new();
    let (foreign_slot, _) = other.slot::<SinkMsg>("sink");

    let mut builder = GraphBuilder::new();
    let (_own_slot, _) = builder.slot::<SinkMsg>("sink");
    builder.define(foreign_slot, Sink { out: out_tx });

    assert!(matches!(
        builder.build(),
        Err(GraphBuildError::InvalidConfig(
            "actor slot belongs to a different graph builder"
        ))
    ));
}

#[derive(Clone)]
struct Park;

impl RawActor for Park {
    type Msg = ();

    async fn run(&mut self, ctx: ActorContext<()>) -> ActorResult {
        ctx.shutdown_token().cancelled().await;
        Ok(())
    }
}

#[derive(Topology)]
struct ParkGraph {
    park: Park,
}

#[tokio::test]
async fn graph_with_applies_builder_config() {
    let mut builder = GraphBuilder::new();
    builder.name("configured");
    builder.mailbox_capacity(1);
    builder.actor_shutdown_timeout(Duration::from_millis(50));

    let mut park = None;
    let graph = ParkGraph::graph_with(builder, |refs| {
        park = Some(refs.park.clone());
        ParkGraph { park: Park }
    })
    .expect("configured graph builds");
    assert_eq!(graph.name(), "configured");

    let park = park.expect("topology closure captured park ref");
    let (stop, tasks) = start_graph(&graph);

    park.send(()).await.expect("first message fits");
    assert!(matches!(
        park.try_send(()),
        Err(SendError::MailboxFull { actor_id }) if actor_id == "park"
    ));

    stop_graph(stop, tasks).await;
}

#[test]
fn graph_with_reports_field_name_collision_with_pre_registered_actor() {
    let mut builder = GraphBuilder::new();
    builder.actor("park", Park);

    match ParkGraph::graph_with(builder, |_| ParkGraph { park: Park }) {
        Err(GraphBuildError::DuplicateActorId { actor_id }) => assert_eq!(actor_id, "park"),
        Ok(_) => panic!("expected DuplicateActorId, got valid graph"),
        Err(error) => panic!("expected DuplicateActorId, got {error:?}"),
    }
}

#[test]
fn empty_slot_name_records_invalid_config_and_detaches() {
    let mut builder = GraphBuilder::new();
    let (slot, actor_ref) = builder.slot::<()>("");
    assert_eq!(actor_ref.id(), "");
    builder.define(slot, Park);
    builder.actor("real", Park);

    match builder.build() {
        Err(GraphBuildError::InvalidConfig(msg)) => {
            assert_eq!(msg, "actor id must not be empty")
        }
        other => panic!("expected InvalidConfig, got {other:?}"),
    }
}

#[test]
fn define_on_duplicate_detached_token_does_not_corrupt_first_slot() {
    let mut builder = GraphBuilder::new();
    let (first_slot, _first_ref) = builder.slot::<()>("park");
    let (dup_slot, _dup_ref) = builder.slot::<()>("park");

    builder.define(first_slot, Park);
    builder.define(dup_slot, Park);

    match builder.build() {
        Err(GraphBuildError::DuplicateActorId { actor_id }) => assert_eq!(actor_id, "park"),
        other => panic!("expected DuplicateActorId, got {other:?}"),
    }
}

#[test]
fn add_skips_explicitly_taken_suffix() {
    let mut builder = GraphBuilder::new();
    builder.actor("Park-2", Park);
    let first = builder.add(Park);
    let second = builder.add(Park);
    assert_eq!(first.id(), "Park");
    assert_eq!(second.id(), "Park-3");
    builder.build().expect("valid graph");
}
