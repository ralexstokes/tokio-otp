use tokio_otp::Topology;

#[derive(Clone)]
struct NotActor;

#[derive(Topology)]
struct BadTopology {
    worker: NotActor,
}

fn main() {}
