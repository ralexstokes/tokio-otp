use tokio_otp::{ActorContext, ActorResult, RawActor, Topology};

#[derive(Clone)]
struct Park;

impl RawActor for Park {
    type Msg = ();

    async fn run(&mut self, _: ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

#[derive(Topology)]
#[topology(bogus)]
struct ParkGraph {
    park: Park,
}

fn main() {}
