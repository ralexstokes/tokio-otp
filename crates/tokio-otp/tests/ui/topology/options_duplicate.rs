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
struct ParkGraph {
    #[topology(
        options = tokio_otp::ActorOptions::new(),
        options = tokio_otp::ActorOptions::new()
    )]
    park: Park,
}

fn main() {}
