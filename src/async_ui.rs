use crate::bridge_request::TickResult;
use crate::system_state::ErasedSystemStateCell;
use crate::{AsyncSystemState, AsyncWorld, async_world_sync_point};
use async_channel::Sender;
use bevy_app::{App, Plugin, PostUpdate};
use bevy_ecs::component::{Component, ComponentId, ComponentMutability, Mutable};
use bevy_ecs::entity::Entity;
use bevy_ecs::lifecycle::HookContext;
use bevy_ecs::prelude::{Changed, Commands, Mut, Query, QueryState, Template, With, World};
use bevy_ecs::query::QueryBuilder;
use bevy_ecs::system::{Populated, SystemParam};
use bevy_ecs::template::{ScopedEntities, ScopedEntityIndex, TemplateContext};
use bevy_ecs::world::DeferredWorld;
use bevy_ecs_macros::{EntityEvent, Resource};
use bevy_scene::{ResolveContext, ResolveSceneError, ResolvedScene, Scene};
use bevy_tasks::AsyncComputeTaskPool;
use futures::FutureExt;
use std::any::{Any, TypeId};
use std::boxed::Box;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, RwLock};
use std::vec;
use std::vec::Vec;

// Unlike the code in the rest of the crate, this code is quite sub-optimal, and has not been written
// with an eye for either readability and conciseness, nor performance and is not, unlike the previous code
// heavily vetted for correctness. While the prior code is almost certainly bug-free and well tested
// the following code is not.
pub struct AsyncUiPlugin;

pub struct AsyncUi;

impl Plugin for AsyncUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(PostUpdate, async_world_sync_point::<AsyncUi>);
        let async_world = app.world().resource::<AsyncWorld>().clone();
        app.init_resource::<MutationTrackingRes>();
        app.insert_resource(Ctx {
            async_world,
            observer_cache: Arc::new(RwLock::new(HashMap::default())),
            system_cache: Arc::new(RwLock::new(HashMap::default())),
        });
    }
}

#[derive(Resource, Clone)]
pub struct Ctx {
    pub(crate) async_world: AsyncWorld,
    observer_cache: Arc<RwLock<HashMap<(Entity, TypeId), Box<dyn Any + Send + Sync + 'static>>>>,
    system_cache: Arc<RwLock<HashMap<TypeId, Arc<dyn ErasedSystemStateCell>>>>,
}

impl Ctx {
    pub fn cached_state<P: SystemParam + 'static>(&self) -> AsyncSystemState<P> {
        if let Some(system_state) = self.system_cache.read().unwrap().get(&TypeId::of::<P>()) {
            AsyncSystemState {
                _p: PhantomData,
                world: self.async_world.clone(),
                system_state: system_state.clone(),
            }
        } else {
            let state = AsyncSystemState::new(self.async_world.clone());
            self.system_cache
                .write()
                .unwrap()
                .insert(TypeId::of::<P>(), state.system_state.clone());
            state
        }
    }
}

pub struct AsyncClosureWrapper2<T: AsyncFnMut(Ctx, Vec<Entity>) + Send + Sync + Clone + 'static>(
    pub T,
    pub Vec<usize>,
);
pub struct AsyncClosureWrapper<T: AsyncFnMut(Ctx, Vec<Entity>) + Send + Sync + Clone + 'static>(
    pub T,
    pub Vec<ScopedEntityIndex>,
);

impl<T: AsyncFnMut(Ctx, Vec<Entity>) + Send + Sync + 'static + core::clone::Clone> Scene
    for AsyncClosureWrapper2<T>
{
    fn resolve(
        self,
        context: &mut ResolveContext,
        scene: &mut ResolvedScene,
    ) -> bevy_ecs::error::Result<(), ResolveSceneError> {
        let closure_wrapper = AsyncClosureWrapper(
            self.0.clone(),
            self.1
                .clone()
                .into_iter()
                .map(|e| ScopedEntityIndex {
                    scope: context.current_entity_scope(),
                    index: e,
                })
                .collect(),
        );
        scene.push_template(closure_wrapper);
        Ok(())
    }
}
impl<T: AsyncFnMut(Ctx, Vec<Entity>) + Send + Sync + Clone + 'static> Clone
    for AsyncClosureWrapper<T>
{
    fn clone(&self) -> Self {
        Self {
            0: self.0.clone(),
            1: self.1.clone(),
        }
    }
}

impl<T: AsyncFnMut(Ctx, Vec<Entity>) + Send + Sync + Clone + 'static> Template
    for AsyncClosureWrapper<T>
{
    type Output = AsyncTaskThing<T>;

    fn build_template(
        &self,
        context: &mut TemplateContext,
    ) -> bevy_ecs::error::Result<Self::Output> {
        let mut entities = vec![];
        for scoped_entity_index in &self.1 {
            entities.push(context.get_scoped_entity(*scoped_entity_index));
        }
        let async_ctx = context.resource::<Ctx>().clone();
        let mut closure = self.0.clone();
        let (tx, rx) = async_channel::bounded(1);
        AsyncComputeTaskPool::get()
            .spawn_local(async move {
                let mut future = closure(async_ctx, entities);
                futures::select! {
                    _ = rx.recv().fuse() => {}
                    _ = future.fuse() => {}
                }
            })
            .detach();
        Ok(AsyncTaskThing::<T>(PhantomData, tx))
    }

    fn clone_template(&self) -> Self {
        self.clone()
    }
}

#[derive(Component)]
#[component(on_despawn)]
pub struct AsyncTaskThing<T: Send + Sync + 'static>(PhantomData<T>, Sender<()>);

impl<T: Send + Sync + 'static> AsyncTaskThing<T> {
    fn on_despawn(world: DeferredWorld, hook_context: HookContext) {
        let _ = world
            .entity(hook_context.entity)
            .get::<AsyncTaskThing<T>>()
            .unwrap()
            .1
            .try_send(());
    }
}

mod async_observers {
    use crate::async_ui::{AsyncUi, Ctx, MutationOccurred, MutationTracker};
    use bevy_ecs::observer::On;
    use bevy_ecs::prelude::{Bundle, Commands, Component, Entity, EntityEvent, Query};
    use crossbeam::channel::{Receiver, Sender};
    use std::any::TypeId;
    use std::boxed::Box;
    use std::marker::PhantomData;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll, Waker};

    #[derive(Component)]
    pub struct AsyncObserver<E: EntityEvent, B: Bundle>(
        Entity,
        Sender<(Arc<AtomicBool>, Waker)>,
        Arc<AtomicBool>,
        PhantomData<(E, B)>,
        bool,
    );
    impl<E: EntityEvent, B: Bundle> Clone for AsyncObserver<E, B> {
        fn clone(&self) -> Self {
            Self {
                0: self.0.clone(),
                1: self.1.clone(),
                2: Arc::new(AtomicBool::new(false)),
                3: PhantomData,
                4: false,
            }
        }
    }
    #[derive(Component)]
    pub struct AsyncCloneObserver<E: EntityEvent + Clone, B: Bundle>(
        Entity,
        Sender<(Sender<(E, Entity)>, Waker)>,
        Option<Receiver<(E, Entity)>>,
        PhantomData<(E, B)>,
    );
    impl<E: EntityEvent + Clone, B: Bundle> Clone for AsyncCloneObserver<E, B> {
        fn clone(&self) -> Self {
            Self {
                0: self.0.clone(),
                1: self.1.clone(),
                2: None,
                3: PhantomData,
            }
        }
    }

    impl Ctx {
        pub async fn on_mutation<C: Component<Mutability = bevy_ecs::component::Mutable>>(
            &self,
            e: Entity,
        ) {
            let state = self.cached_state::<(Commands, Query<&MutationTracker<C>>)>();
            state
                .bridge(AsyncUi, |(mut commands, query)| {
                    if query.contains(e) {
                        return;
                    }
                    commands.entity(e).insert(MutationTracker::<C>(PhantomData));
                })
                .await
                .unwrap();
            self.on::<MutationOccurred<C>>(e).await;
        }
        pub async fn on_cloned_uncached<E: EntityEvent + Clone, B: Bundle>(
            &self,
            e: Entity,
        ) -> AsyncCloneObserver<E, B> {
            let state = self.cached_state::<(Commands, Query<&AsyncCloneObserver<E, B>>)>();
            state
                .bridge(AsyncUi, move |(mut commands, q)| {
                    if let Ok(async_observer) = q.get(e) {
                        return async_observer.clone();
                    }
                    let (tx, rx) = crossbeam::channel::unbounded();
                    let async_observer = AsyncCloneObserver {
                        0: e,
                        1: tx,
                        2: None,
                        3: PhantomData,
                    };
                    commands.entity(e).observe(move |on: On<E, B>| {
                        for (notify, waker) in rx.try_iter() {
                            let _ = notify.send((on.event().clone(), on.observer()));
                            waker.wake();
                        }
                    });
                    async_observer
                })
                .await
                .unwrap()
        }
        pub async fn on_cloned<E: EntityEvent + Clone, B: Bundle>(&self, e: Entity) -> E {
            let async_observer;
            if !self
                .observer_cache
                .read()
                .unwrap()
                .contains_key(&(e, TypeId::of::<AsyncCloneObserver<E, B>>()))
            {
                async_observer = self
                    .cached_state::<(Commands, Query<&AsyncCloneObserver<E, B>>)>()
                    .bridge(AsyncUi, move |(mut commands, q)| {
                        if let Ok(async_observer) = q.get(e) {
                            return async_observer.clone();
                        }
                        let (tx, rx) = crossbeam::channel::unbounded();
                        let async_observer = AsyncCloneObserver {
                            0: e,
                            1: tx,
                            2: None,
                            3: PhantomData,
                        };
                        commands.entity(e).observe(move |on: On<E, B>| {
                            for (notify, waker) in rx.try_iter() {
                                let _ = notify.send((on.event().clone(), on.observer()));
                                waker.wake();
                            }
                        });
                        async_observer
                    })
                    .await
                    .unwrap();
                self.observer_cache.write().unwrap().insert(
                    (e, TypeId::of::<AsyncCloneObserver<E, B>>()),
                    Box::new(async_observer.clone()),
                );
            } else {
                async_observer = self
                    .observer_cache
                    .read()
                    .unwrap()
                    .get(&(e, TypeId::of::<AsyncCloneObserver<E, B>>()))
                    .unwrap()
                    .downcast_ref::<AsyncCloneObserver<E, B>>()
                    .unwrap()
                    .clone();
            }
            async_observer.await.0
        }
        pub async fn on_uncached<E: EntityEvent, B: Bundle>(
            &self,
            e: Entity,
        ) -> AsyncObserver<E, B> {
            let state = self.cached_state::<(Commands, Query<&AsyncObserver<E, B>>)>();
            state
                .bridge(AsyncUi, move |(mut commands, q)| {
                    if let Ok(async_observer) = q.get(e) {
                        return async_observer.clone();
                    }
                    let (tx, rx) = crossbeam::channel::unbounded();
                    let async_observer = AsyncObserver {
                        0: e,
                        1: tx,
                        2: Arc::new(AtomicBool::new(false)),
                        3: PhantomData,
                        4: false,
                    };
                    commands.entity(e).observe(move |_on: On<E, B>| {
                        for (notify, waker) in rx.try_iter() {
                            notify.store(true, Ordering::Relaxed);
                            waker.wake();
                        }
                    });
                    async_observer
                })
                .await
                .unwrap()
        }
        pub async fn on<E: EntityEvent>(&self, e: Entity) {
            self.on_with::<E, ()>(e).await
        }
        pub async fn on_with<E: EntityEvent, B: Bundle>(&self, e: Entity) {
            let async_observer;
            if !self
                .observer_cache
                .read()
                .unwrap()
                .contains_key(&(e, TypeId::of::<AsyncObserver<E, B>>()))
            {
                async_observer = self
                    .cached_state::<(Commands, Query<&AsyncObserver<E, B>>)>()
                    .bridge(AsyncUi, move |(mut commands, q)| {
                        if let Ok(async_observer) = q.get(e) {
                            return async_observer.clone();
                        }
                        let (tx, rx) = crossbeam::channel::unbounded();
                        let async_observer = AsyncObserver {
                            0: e,
                            1: tx,
                            2: Arc::new(AtomicBool::new(false)),
                            3: PhantomData,
                            4: false,
                        };
                        commands.entity(e).observe(move |_on: On<E, B>| {
                            for (notify, waker) in rx.try_iter() {
                                notify.store(true, Ordering::Relaxed);
                                waker.wake();
                            }
                        });
                        async_observer
                    })
                    .await
                    .unwrap();
                self.observer_cache.write().unwrap().insert(
                    (e, TypeId::of::<AsyncObserver<E, B>>()),
                    Box::new(async_observer.clone()),
                );
            } else {
                async_observer = self
                    .observer_cache
                    .read()
                    .unwrap()
                    .get(&(e, TypeId::of::<AsyncObserver<E, B>>()))
                    .unwrap()
                    .downcast_ref::<AsyncObserver<E, B>>()
                    .unwrap()
                    .clone();
            }
            async_observer.await;
        }
    }

    impl<E: EntityEvent, B: Bundle> Unpin for AsyncObserver<E, B> {}

    impl<E: EntityEvent, B: Bundle> Future for AsyncObserver<E, B> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.2.load(Ordering::Relaxed) {
                return Poll::Ready(());
            }
            if self.4 == false {
                self.1.send((self.2.clone(), cx.waker().clone())).unwrap();
                self.4 = true;
            }
            return Poll::Pending;
        }
    }

    impl<E: EntityEvent + Clone, B: Bundle> Unpin for AsyncCloneObserver<E, B> {}

    impl<E: EntityEvent + Clone, B: Bundle> Future for AsyncCloneObserver<E, B> {
        type Output = (E, Entity);

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.2.clone() {
                None => {
                    let (tx, rx) = crossbeam::channel::bounded(1);
                    self.2 = Some(rx);
                    self.1.send((tx, cx.waker().clone())).unwrap();
                    Poll::Pending
                }
                Some(awa) => {
                    if let Ok(output) = awa.try_recv() {
                        Poll::Ready(output)
                    } else {
                        Poll::Pending
                    }
                }
            }
        }
    }
}

#[derive(Component)]
#[require(MutationSender)]
#[component(on_add)]
#[component(on_remove)]
pub struct MutationTracker<C: Component<Mutability = Mutable>>(PhantomData<C>);

impl<C: Component<Mutability = Mutable>> MutationTracker<C> {
    fn on_add(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
        let component_id = world.component_id::<C>().unwrap();
        world
            .entity_mut(entity)
            .get_mut::<MutationSender>()
            .unwrap()
            .0
            .insert(component_id, |entity: Entity, world: &mut World| {
                world.trigger(MutationOccurred::<C> {
                    entity,
                    phantom: PhantomData,
                });
            });
        world.commands().queue(move |world: &mut World| {
            let query_state = QueryBuilder::<Entity, Changed<C>>::new(world).build();

            impl<C2: Component<Mutability = Mutable>> MyIter for QueryState<Entity, Changed<C2>> {
                fn entities(&mut self, world: &mut World) -> Vec<Entity> {
                    self.iter(world).collect()
                }
            }
            world
                .resource_mut::<MutationTrackingRes>()
                .0
                .insert(component_id, Box::new(query_state));
        });
    }
    fn on_remove(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
        world.commands().queue(|world: &mut World| {
            if world
                .run_system_cached(|_mutation_tracker: Populated<&MutationTracker<C>>| {})
                .is_err()
            {
                let component_id = world.component_id::<C>().unwrap();
                world
                    .resource_mut::<MutationTrackingRes>()
                    .0
                    .remove(&component_id);
            }
        })
    }
}
trait MyIter {
    fn entities(&mut self, world: &mut World) -> Vec<Entity>;
}
#[derive(Resource, Default)]
pub struct MutationTrackingRes(HashMap<ComponentId, Box<dyn MyIter + Send + Sync>>);

#[derive(EntityEvent)]
struct MutationOccurred<C: Component<Mutability = Mutable>> {
    entity: Entity,
    phantom: PhantomData<C>,
}
#[derive(Component, Default)]
struct MutationSender(HashMap<ComponentId, fn(Entity, &mut World)>);

pub fn pump_mutation_observers(world: &mut World) -> TickResult {
    let mut tick_result = TickResult::NoWork;
    world.resource_scope(
        |world: &mut World, mut mutation_tracking_res: Mut<MutationTrackingRes>| {
            for (component, query) in mutation_tracking_res.0.iter_mut() {
                for item in query.entities(world) {
                    let awa = *world
                        .entity(item)
                        .get::<MutationSender>()
                        .unwrap()
                        .0
                        .get(&component)
                        .unwrap();
                    awa(item, world);
                    tick_result = TickResult::DidWork
                }
            }
        },
    );
    tick_result
}
