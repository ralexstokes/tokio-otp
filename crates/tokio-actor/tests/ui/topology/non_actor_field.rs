use tokio_actor::Topology;

#[derive(Clone)]
struct NotActor;

#[derive(Topology)]
struct BadTopology {
    worker: NotActor,
}

fn main() {}
