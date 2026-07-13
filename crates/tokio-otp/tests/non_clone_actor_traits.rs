use std::sync::Mutex;

use tokio_otp::{Actor, ActorContext, ActorResult, RawActor};

struct HandlerWithNonCloneState {
    _state: Mutex<()>,
}

impl Actor for HandlerWithNonCloneState {
    type Msg = ();

    async fn handle(&mut self, (): (), _ctx: &ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

struct RawWithNonCloneState {
    _state: Mutex<()>,
}

impl RawActor for RawWithNonCloneState {
    type Msg = ();

    async fn run(&mut self, _ctx: ActorContext<()>) -> ActorResult {
        Ok(())
    }
}

fn assert_actor<T: Actor>() {}
fn assert_raw_actor<T: RawActor>() {}

#[test]
fn actor_traits_accept_non_clone_state() {
    assert_actor::<HandlerWithNonCloneState>();
    assert_raw_actor::<HandlerWithNonCloneState>();
    assert_raw_actor::<RawWithNonCloneState>();
}
