use std::marker::PhantomData;

use tokio_actor::{ActorContext, ActorRegistry, ActorResult, GraphBuilder, LookupError, RawActor};

struct Drain<M>(PhantomData<fn(M)>);

impl<M> Drain<M> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<M> Clone for Drain<M> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<M: Send + 'static> RawActor for Drain<M> {
    type Msg = M;

    async fn run(&self, mut ctx: ActorContext<M>) -> ActorResult {
        while ctx.recv().await.is_some() {}
        Ok(())
    }
}

#[test]
fn registry_tracks_registered_actor_refs() {
    let mut builder = GraphBuilder::new();
    builder.actor("worker", Drain::<u32>::new());
    let graph = builder.build().expect("valid graph");

    let actor_set = graph.into_actor_set().expect("graph decomposes");
    let worker = actor_set.actor("worker").expect("worker exists");
    let registry = ActorRegistry::new();

    worker
        .register_with(&registry)
        .expect("worker registers successfully");

    assert!(registry.contains("worker"));
    assert_eq!(registry.actor_ids(), vec!["worker".to_owned()]);
    assert_eq!(registry.stats(), vec![worker.stats()]);
    assert_eq!(
        registry
            .actor_ref::<u32>("worker")
            .expect("actor ref exists")
            .id(),
        "worker"
    );
    assert!(matches!(
        registry.actor_ref::<String>("worker"),
        Err(LookupError::MessageTypeMismatch { .. })
    ));

    registry
        .deregister("worker")
        .expect("worker deregisters successfully");
    assert!(!registry.contains("worker"));
    assert!(matches!(
        registry.actor_ref::<u32>("worker"),
        Err(LookupError::UnknownActor { .. })
    ));
}
