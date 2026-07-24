use tokio_otp::{ActorContext, ActorResult, RawActor, Topology};

#[derive(Clone)]
struct Park;

impl RawActor for Park {
    type Msg = ();

    async fn run(&mut self, _: ActorContext<()>) -> ActorResult {
        Ok(tokio_otp::prelude::Continue)
    }
}

#[derive(Topology)]
struct ParkGraph {
    #[topology(options)]
    park: Park,
}

fn main() {}
