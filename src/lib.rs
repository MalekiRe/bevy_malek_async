use crate::keyed_queues::KeyedQueues;
use bevy_ecs::{
    change_detection::Tick,
    error::ErrorContext,
    prelude::NonSend,
    schedule::{InternedScheduleLabel, ScheduleLabel},
    system::{SystemParam, SystemParamValidationError, SystemState},
    world::{World, WorldId, unsafe_world_cell::UnsafeWorldCell},
};
use bevy_platform::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, RwLock},
};
use concurrent_queue::ConcurrentQueue;
use core::{
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use std::sync::Condvar;

pub struct AsyncEcsPlugin;

#[derive(Debug)]
struct State {
    connected: isize,
    waiting_for: isize,
}

#[derive(Debug, Clone)]
pub struct WaitBarrier {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    mu: Mutex<State>,
    cv: Condvar,
}

impl WaitBarrier {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                mu: Mutex::new(State {
                    connected: 0,
                    waiting_for: 0,
                }),
                cv: Condvar::new(),
            }),
        }
    }

    /// Create a teller (a "boi") linked to this barrier.
    pub fn new_wait_teller(&self) -> WaitTeller {
        let mut st = self.inner.mu.lock().unwrap();
        st.connected += 1;
        // Note: do NOT touch waiting_for here. A new teller doesn't retroactively
        // affect already-issued waits; it only affects future wait() calls.
        drop(st);

        WaitTeller {
            inner: self.inner.clone(),
            // Optional safety: prevent double-drop bookkeeping if you add an explicit disconnect().
            alive: true,
        }
    }

    /// Wait for *one signal from each currently-connected teller*.
    ///
    /// Semantics: increments `waiting_for` by `connected` and blocks until
    /// enough `signal()`/drops have happened to make `waiting_for <= 0`.
    pub fn wait(&self) {
        let mut st = self.inner.mu.lock().unwrap();

        // If nobody is connected, this wait is trivially satisfied.
        if st.connected <= 0 {
            return;
        }

        st.waiting_for += st.connected;

        while st.waiting_for > 0 {
            st = self.inner.cv.wait(st).unwrap();
        }
    }

    /// Non-blocking: "issue" a wait demand, returning how many signals are now required.
    /// (Handy for debugging/metrics.)
    pub fn issue_wait(&self) -> isize {
        let mut st = self.inner.mu.lock().unwrap();
        if st.connected > 0 {
            st.waiting_for += st.connected;
        }
        st.waiting_for
    }

    /// Debug/metrics helpers (optional)
    pub fn connected(&self) -> isize {
        self.inner.mu.lock().unwrap().connected
    }
    pub fn waiting_for(&self) -> isize {
        self.inner.mu.lock().unwrap().waiting_for
    }
}

#[derive(Debug)]
pub struct WaitTeller {
    inner: Arc<Inner>,
    alive: bool,
}

impl WaitTeller {
    /// One "unit" of progress (finished / pending / poll complete, etc.).
    pub fn signal(&self) {
        let mut st = self.inner.mu.lock().unwrap();
        st.waiting_for -= 1;

        // If this satisfies current waits, wake everyone.
        if st.waiting_for <= 0 {
            self.inner.cv.notify_all();
        }
    }

    /// Optional explicit disconnect (so you can drop later without double bookkeeping).
    pub fn disconnect(&mut self) {
        self.do_drop_bookkeeping();
        // prevent Drop from doing it again
        self.alive = false;
    }

    fn do_drop_bookkeeping(&mut self) {
        let mut st = self.inner.mu.lock().unwrap();
        st.connected -= 1;

        // Dropping reduces the number of signals we should still expect
        // from "currently outstanding" waits.
        st.waiting_for -= 1;

        if st.waiting_for <= 0 {
            self.inner.cv.notify_all();
        }
    }
}

impl Drop for WaitTeller {
    fn drop(&mut self) {
        if self.alive {
            self.do_drop_bookkeeping();
        }
    }
}

impl bevy_app::Plugin for AsyncEcsPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        use bevy_app::prelude::{
            First, FixedFirst, FixedLast, FixedPostUpdate, FixedPreUpdate, FixedUpdate, Last,
            PostStartup, PostUpdate, PreStartup, PreUpdate, Startup, Update,
        };
        for awa in vec![
            PreStartup.intern(),
            Startup.intern(),
            PostStartup.intern(),
            PreUpdate.intern(),
            Update.intern(),
            PostUpdate.intern(),
            FixedPostUpdate.intern(),
            FixedPreUpdate.intern(),
            FixedUpdate.intern(),
            First.intern(),
            Last.intern(),
            FixedFirst.intern(),
            FixedLast.intern(),
        ] {
            app.add_systems(awa, move |world: &mut World| {
                run_async_ecs_on_schedule(awa, world);
            });
        }
    }
}

pub fn run_async_ecs_on_schedule(schedule: InternedScheduleLabel, world: &mut World) {
    GLOBAL_WAKE_REGISTRY.wait(schedule, world);
}

/// Keyed queues is a combination of a hashmap and a concurrent queue which is useful because it
/// allows for non-blocking keyed queues.
/// We want every World's async machinery to be as independent as possible, and this allows us
/// to key our Queues on `(WorldId, Schedule)` so that there is 0 contention on the fast path and
/// arbitrary N number of worlds running in parallel on the same process do not interfere at all
/// except the very first time a new world initializes it's key.
mod keyed_queues {
    use bevy_platform::collections::HashMap;
    use bevy_platform::sync::{Arc, RwLock};
    use concurrent_queue::ConcurrentQueue;
    use core::hash::Hash;
    /// `HashMap<K, Arc<ConcurrentQueue<V>>>` behind a single `RwLock`.
    /// - Writers only contend when creating a new key.
    /// - `push` is almost always non-blocking (unbounded queue).
    pub struct KeyedQueues<K, V> {
        inner: RwLock<HashMap<K, Arc<ConcurrentQueue<V>>>>,
    }

    impl<K, V> KeyedQueues<K, V>
    where
        K: Eq + Hash + Clone,
        V: Send + 'static,
    {
        pub fn new() -> Self {
            Self {
                inner: RwLock::new(HashMap::new()),
            }
        }

        #[inline]
        pub fn get_or_create(&self, key: &K) -> Arc<ConcurrentQueue<V>> {
            // Fast path: try read lock first
            if let Some(q) = self.inner.read().unwrap().get(key).cloned() {
                return q;
            }
            // Slow path: create under write lock if still absent
            let mut write = self.inner.write().unwrap();
            // We intentionally check a second time because of synchronization
            if let Some(q) = write.get(key).cloned() {
                return q;
            }
            let q = Arc::new(ConcurrentQueue::unbounded());
            write.insert(key.clone(), q.clone());
            q
        }

        /// Potentially-blocking send but almost never blocking (unbounded queue => `push` never fails).
        /// ( Only blocks when the `(WorldId, Schedule)` has never been used before
        #[inline]
        pub fn try_send(&self, key: &K, val: V) -> Result<(), concurrent_queue::PushError<V>> {
            let q = self.get_or_create(key);
            q.push(val)
        }
    }
}

/// This is an abstraction that temporarily and soundly stores the `UnsafeWorldCell` in a static so we can access
/// it from any async task, runtime, and thread.
static GLOBAL_WORLD_ACCESS: WorldAccessRegistry = WorldAccessRegistry(OnceLock::new());

/// The entrypoint, stores `Waker`s from `async_access`'s that wish to be polled with world access
/// also stores the generic function pointer to the concrete function that initializes the
/// system state for any set of `SystemParams`
pub(crate) static GLOBAL_WAKE_REGISTRY: WakeRegistry = WakeRegistry(OnceLock::new());

/// Is the `GLOBAL_WAKE_REGISTRY`
pub(crate) struct WakeRegistry(
    OnceLock<(
        KeyedQueues<(WorldId, InternedScheduleLabel), EcsTaskStateTransition<Uninitialized>>,
        KeyedQueues<(WorldId, InternedScheduleLabel), EcsTaskStateTransition<ReadyToWake>>,
    )>,
);

pub(crate) struct WaitBarrierRegistry(
    OnceLock<RwLock<HashMap<(WorldId, InternedScheduleLabel), Arc<WaitBarrier>>>>,
);

const WAIT_BARRIER_REGISTRY: WaitBarrierRegistry = WaitBarrierRegistry(OnceLock::new());

impl WakeRegistry {
    /// This function finds all pending `async_access` calls for a particular `Schedule` and a particular
    /// `WorldId`. It wakes all of them, temporarily and soundly stores a `UnsafeWorldCell` in the
    /// `GLOBAL_WORLD_ACCESS` and parks until the tasks it has awoken either complete their `async_access`
    /// or have returned `Poll::Pending` for a variety of reasons.
    /// The performance implications of this call are entirely dependent on the async runtime
    /// you are using it with, certain poor implementations *could* cause this to take longer
    /// than expect to resolve.
    /// Returns `Some` as long as the last call processed any number of waiting `async_access` calls.
    pub fn wait(&self, schedule: InternedScheduleLabel, world: &mut World) -> Option<()> {
        let world_id = world.id();
        let wait_barrier = WAIT_BARRIER_REGISTRY
            .0
            .get_or_init(|| RwLock::new(HashMap::new()))
            .read()
            .unwrap()
            .get(&(world_id, schedule))
            .cloned()?;
        let global_wake_registry = GLOBAL_WAKE_REGISTRY
            .0
            .get_or_init(|| (KeyedQueues::new(), KeyedQueues::new()));
        if global_wake_registry
            .0
            .get_or_create(&(world_id, schedule))
            .is_empty()
            && global_wake_registry
                .1
                .get_or_create(&(world_id, schedule))
                .is_empty()
        {
            return None;
        }
        let mut ecs_tasks = bevy_platform::prelude::vec![];
        while let Ok(ecs_task) = global_wake_registry
            .0
            .get_or_create(&(world_id, schedule))
            .pop()
        {
            ecs_tasks.push(ecs_task.initialize(world));
        }
        while let Ok(ecs_task) = global_wake_registry
            .1
            .get_or_create(&(world_id, schedule))
            .pop()
        {
            ecs_tasks.push(ecs_task)
        }
        let mut need_to_apply_system_state = None;
        GLOBAL_WORLD_ACCESS.set(world, || {
            let ecs_tasks: Vec<_> = ecs_tasks
                .into_iter()
                .map(EcsTaskStateTransition::<ReadyToWake>::wake)
                .collect();
            let ecs_tasks = wait_barrier.wait_for_async_tasks(ecs_tasks);
            need_to_apply_system_state = Some(ecs_tasks);
        })?;
        // Applies all the commands stored up to the world and other system state
        for task in need_to_apply_system_state? {
            task.apply_system_params(world);
        }
        Some(())
    }
}

struct ErasedSystemStateQueue {
    thing: *const (),
    drop_func: fn(*const ()),
    clone_func: fn(*const ()) -> ErasedSystemStateQueue,
}
impl ErasedSystemStateQueue {
    fn new<T: SystemParam + 'static>(t: Arc<ConcurrentQueue<SystemState<T>>>) -> Self {
        fn drop_func<T: SystemParam + 'static>(thing: *const ()) {
            unsafe { Arc::from_raw(thing as *const ConcurrentQueue<SystemState<T>>) };
        }
        fn clone_func<T: SystemParam + 'static>(thing: *const ()) -> ErasedSystemStateQueue {
            let our_reference =
                unsafe { Arc::from_raw(thing as *const ConcurrentQueue<SystemState<T>>) };
            // does an extra arc clone to match with the extra drop.
            drop(Arc::into_raw(our_reference.clone()));
            ErasedSystemStateQueue {
                thing,
                drop_func: drop_func::<T>,
                clone_func: clone_func::<T>,
            }
        }
        Self {
            thing: Arc::into_raw(t) as *const (),
            drop_func: drop_func::<T>,
            clone_func: clone_func::<T>,
        }
    }
    unsafe fn clone_into<T: SystemParam + 'static>(&self) -> Arc<ConcurrentQueue<SystemState<T>>> {
        let cloned = (self.clone_func)(self.thing);
        unsafe { Arc::from_raw(cloned.thing as *const ConcurrentQueue<SystemState<T>>) }
    }
}
impl Clone for ErasedSystemStateQueue {
    fn clone(&self) -> Self {
        (self.clone_func)(self.thing)
    }
}
impl Drop for ErasedSystemStateQueue {
    fn drop(&mut self) {
        (self.drop_func)(self.thing);
    }
}
unsafe impl Send for ErasedSystemStateQueue {}
unsafe impl Sync for ErasedSystemStateQueue {}

struct Uninitialized {
    system_init_func: fn(&mut World, ErasedSystemStateQueue),
}

struct ReadyToWake;

struct Awoken;

struct NeedToApplySystemState;

struct EcsTaskStateTransition<T> {
    state_transition_resources: T,
    system_apply_func: fn(&mut World, ErasedSystemStateQueue),
    erased: ErasedSystemStateQueue,
    waker: Waker,
}

impl EcsTaskStateTransition<Uninitialized> {
    fn initialize(self, world: &mut World) -> EcsTaskStateTransition<ReadyToWake> {
        (self.state_transition_resources.system_init_func)(world, self.erased.clone());
        let Self {
            system_apply_func,
            erased,
            waker,
            ..
        } = self;
        EcsTaskStateTransition {
            state_transition_resources: ReadyToWake,
            system_apply_func,
            erased,
            waker,
        }
    }
}

impl EcsTaskStateTransition<ReadyToWake> {
    fn wake(self) -> EcsTaskStateTransition<Awoken> {
        self.waker.wake_by_ref();
        let Self {
            system_apply_func,
            erased,
            waker,
            ..
        } = self;
        EcsTaskStateTransition {
            state_transition_resources: Awoken,
            system_apply_func,
            erased,
            waker,
        }
    }
}

impl EcsTaskStateTransition<NeedToApplySystemState> {
    fn apply_system_params(self, world: &mut World) {
        (self.system_apply_func)(world, self.erased.clone());
    }
}

impl WaitBarrier {
    pub fn wait_for_async_tasks(
        &self,
        ecs_tasks: Vec<EcsTaskStateTransition<Awoken>>,
    ) -> Vec<EcsTaskStateTransition<NeedToApplySystemState>> {
        self.wait();
        ecs_tasks
            .into_iter()
            .map(
                |EcsTaskStateTransition::<Awoken> {
                     system_apply_func,
                     erased,
                     waker,
                     ..
                 }| EcsTaskStateTransition {
                    state_transition_resources: NeedToApplySystemState,
                    system_apply_func,
                    erased,
                    waker,
                },
            )
            .collect()
    }
}

// We have a couple of transitions
// We have acquirin

/// This is a very low contention, no contention in the normal execution path, way of storing and
/// using a `UnsafeWorldCell` from any thread/async task/async runtime.
/// The `Mutex<PhantomData<>>` is used to return `Poll::Pending` early from an `async_access` if
/// another `async_access` is currently using it.
pub(crate) struct WorldAccessRegistry(
    OnceLock<
        RwLock<
            HashMap<
                WorldId,
                RwLock<
                    Option<(
                        UnsafeWorldCell<'static>,
                        Mutex<PhantomData<UnsafeWorldCell<'static>>>,
                    )>,
                >,
            >,
        >,
    >,
);

impl WorldAccessRegistry {
    /// During this `func: FnOnce()` call, calling `get` will access the stored `UnsafeWorldCell`
    fn set(&self, world: &mut World, func: impl FnOnce()) -> Option<()> {
        let this = self.0.get_or_init(|| RwLock::new(HashMap::new()));
        let world_id = world.id();
        if !this.read().unwrap().contains_key(&world_id) {
            // VERY rare only happens the first time we try to do anything async in a new World
            let _ = this.write().unwrap().insert(world_id, RwLock::new(None));
        }

        struct ClearOnDropGuard<'a> {
            slot: &'a RwLock<
                Option<(
                    UnsafeWorldCell<'static>,
                    Mutex<PhantomData<UnsafeWorldCell<'static>>>,
                )>,
            >,
        }
        impl<'a> Drop for ClearOnDropGuard<'a> {
            fn drop(&mut self) {
                // clear it on the way out
                // we can't actually panic here because panicking in a drop is bad
                match self.slot.write() {
                    Ok(mut slot) => {
                        let _ = slot.take();
                    }
                    Err(_) => {
                        // This is okay because the mutex is poisoned so nothing can access the
                        // UnsafeWorldCell now.
                    }
                }
            }
        }
        // SAFETY: This mem transmute is safe only because we drop it after, and our GLOBAL_WORLD_ACCESS is private, and we don't clone it
        // where we do use it, so the lifetime doesn't get propagated anywhere.
        // Lifetimes are not used in any actual code optimization, so turning it into a static does not violate any of rust's rules
        // As *LONG* as we keep it within it's lifetime, which we do here, manually, with our `ClearOnDrop` struct.
        unsafe {
            let binding = this.read().unwrap();
            let world_container = binding.get(&world_id).unwrap();
            // SAFETY this is required in order to make sure that even in the event of a panic, this can't get accessed
            let _clear = ClearOnDropGuard {
                slot: world_container,
            };
            // SAFETY: This mem transmute is safe only because we drop it after, and our GLOBAL_WORLD_ACCESS is private, and we don't clone it
            // where we do use it, so the lifetime doesn't get propagated anywhere.
            // Lifetimes are not used in any actual code optimization, so turning it into a static does not violate any of rust's rules
            // As *LONG* as we keep it within it's lifetime, which we do here, manually, with our `ClearOnDrop` struct.
            world_container.write().unwrap().replace((
                core::mem::transmute::<UnsafeWorldCell, UnsafeWorldCell<'static>>(
                    world.as_unsafe_world_cell(),
                ),
                Mutex::new(PhantomData),
            ));
            func();
        }
        Some(())
    }
    fn get<T>(
        &self,
        world_id: WorldId,
        func: impl FnOnce(UnsafeWorldCell) -> Poll<T>,
    ) -> Option<Poll<T>> {
        // it's okay to *not* do the RaiiThing on these early returns, because that means we aren't in a state
        // where a thread is parked because of our world.
        let a = self.0.get()?.read().unwrap();
        let b = a.get(&world_id)?.read().unwrap();
        let our_thing = b.as_ref()?;

        // this allows us to effectively yield as if pending if the world doesn't exist rn.
        let _world = our_thing.1.try_lock().ok()?;
        // SAFETY: this is safe because we ensure no one else has access to the world.
        Some(func(our_thing.0))
    }
}

impl<P: bevy_ecs::system::SystemParam + 'static> EcsTask<P> {
    pub async fn run_system<Func, Out>(self, schedule: impl ScheduleLabel, ecs_access: Func) -> Out
    where
        for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
    {
        async_access(self, schedule, ecs_access).await
    }
}

pub trait CreateEcsTask {
    fn ecs_task<P: SystemParam + 'static>(self) -> EcsTask<P>;
}

impl CreateEcsTask for WorldId {
    fn ecs_task<P: SystemParam + 'static>(self) -> EcsTask<P> {
        EcsTask::new(self)
    }
}

#[rustfmt::skip]
/// Allows you to access the ECS from any arbitrary async runtime.
/// Calls will never return immediately and will always start Pending at least once.
/// Call this with the same `EcsTask` to persist `SystemParams` like `Local` or `Changed`
/// Just use `world_id` if you do not mind a new `SystemParam` being initialized every time.
async fn async_access<P, Func, Out>(
    task_identifier: impl Into<EcsTask<P>>,
    schedule: impl ScheduleLabel,
    ecs_access: Func,
) -> Out
where
    P: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
{
    let task_identifier = task_identifier.into();
    let temp = WAIT_BARRIER_REGISTRY.0;
    let wait_barrier_registry = temp.get_or_init(|| RwLock::new(HashMap::new()));
    let world_id_schedule = (task_identifier.0.0, schedule.intern());
    if !wait_barrier_registry.read().unwrap().contains_key(&world_id_schedule) {
        wait_barrier_registry.write().unwrap().insert(world_id_schedule, Arc::new(WaitBarrier::new()));
    }
    let wait_teller = wait_barrier_registry.read().unwrap().get(&world_id_schedule).unwrap().new_wait_teller();
    PendingEcsCall::<P, Func, Out>(
        PhantomData::<P>,
        PhantomData,
        Some(ecs_access),
        world_id_schedule,
        wait_teller,
        Arc::new(ConcurrentQueue::bounded(1)),
        FutureState::Uninitialized,
    )
    .await
}

#[derive(PartialOrd, PartialEq, Eq, Ord, Hash, Debug, Copy, Clone)]
enum FutureState {
    Initialized,
    Uninitialized,
}

impl<P: SystemParam + 'static> From<WorldId> for EcsTask<P> {
    fn from(value: WorldId) -> Self {
        EcsTask(Arc::new(InternalEcsTask(value, PhantomData)))
    }
}

/// An `EcsTask` can be re-used in order to persist `SystemParams` like `Local`, `Changed`, or `Added`
pub struct EcsTask<P: SystemParam + 'static>(Arc<InternalEcsTask<P>>);

struct InternalEcsTask<P: SystemParam + 'static>(WorldId, PhantomData<P>);

impl<P: SystemParam + 'static> Clone for EcsTask<P> {
    fn clone(&self) -> Self {
        EcsTask(self.0.clone())
    }
}
impl<P: SystemParam + 'static> EcsTask<P> {
    /// Generates a new unique `EcsTask` that can be re-used in order to persist `SystemParams`
    /// like `Local`, `Changed`, or `Added`
    pub fn new(world_id: WorldId) -> Self {
        Self(Arc::new(InternalEcsTask(world_id, PhantomData)))
    }
}

struct PendingEcsCall<P: SystemParam + 'static, Func, Out>(
    PhantomData<P>,
    PhantomData<Out>,
    Option<Func>,
    (WorldId, InternedScheduleLabel),
    WaitTeller,
    Arc<ConcurrentQueue<SystemState<P>>>,
    FutureState,
);

impl<P: SystemParam + 'static, Func, Out> Unpin for PendingEcsCall<P, Func, Out> {}

impl<P, Func, Out> Future for PendingEcsCall<P, Func, Out>
where
    P: SystemParam + 'static,
    for<'w, 's> Func: FnOnce(P::Item<'w, 's>) -> Out,
{
    type Output = Out;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        fn system_state_init<P: SystemParam + 'static>(
            world: &mut World,
            system_state_queue: ErasedSystemStateQueue,
        ) {
            let system_state_queue = unsafe {
                Arc::from_raw(system_state_queue.thing as *const ConcurrentQueue<SystemState<P>>)
            };
            match system_state_queue.push(SystemState::<P>::new(world)) {
                Ok(_) => {}
                Err(_) => panic!(),
            }
        }
        fn system_state_apply<P: SystemParam + 'static>(
            world: &mut World,
            system_state_queue: ErasedSystemStateQueue,
        ) {
            let system_state_queue = unsafe {
                Arc::from_raw(system_state_queue.thing as *const ConcurrentQueue<SystemState<P>>)
            };
            let Ok(mut system_state) = system_state_queue.pop() else {
                panic!()
            };
            system_state.apply(world);
            match system_state_queue.push(system_state) {
                Ok(_) => {}
                Err(_) => panic!(),
            }
        }
        let world_id = self.3.0;

        match GLOBAL_WORLD_ACCESS.get(world_id, |world: UnsafeWorldCell| {
            // SAFETY: We have a fake-mutex around our world, so no one else can do mutable access to it.
            let Ok(mut system_state) = self.5.pop() else {
                return Poll::Pending;
            };
            let out;
            // SAFETY: This is safe because we have a fake-mutex around our world cell, so only one thing can have access to it at a time.
            unsafe {
                let default_error_handler = world.default_error_handler();
                // Obtain params and immediately consume them with the closure,
                // ensuring the borrow ends before `apply`.
                if let Err(err) = SystemState::validate_param(&mut system_state, world) {
                    default_error_handler(
                        err.into(),
                        ErrorContext::System {
                        name: system_state.meta().name().clone(),
                        last_run: /*system_state.meta().last_run*/ Tick::new(0),
                    },
                    );
                }
                if !system_state.meta().is_send() {
                    default_error_handler(
                        SystemParamValidationError::invalid::<NonSend<()>>(
                            "Cannot have your system be non-send / exclusive",
                        )
                        .into(),
                        ErrorContext::System {
                        name: system_state.meta().name().clone(),
                        last_run: /*system_state.meta.last_run */Tick::new(0),
                    },
                    );
                }
                let state = system_state.get_unchecked(world);
                out = self.as_mut().2.take().unwrap()(state);
            }
            match self.5.push(system_state) {
                Ok(_) => {}
                Err(_) => panic!(),
            }
            self.4.disconnect();
            Poll::Ready(out)
        }) {
            Some(Poll::Ready(out)) => Poll::Ready(out),
            _ => {
                // This must be a static, sadly, because we must always make sure that we can store
                // our pending wakers no matter what. Everything else that we care about can be
                // stored on the world itself, but this must always be accessible, even if another
                // `async_access` is currently running.
                let global_wake_registry = GLOBAL_WAKE_REGISTRY
                    .0
                    .get_or_init(|| (KeyedQueues::new(), KeyedQueues::new()));
                match self.6 {
                    FutureState::Initialized => {
                        match global_wake_registry.1.try_send(
                            &self.3,
                            EcsTaskStateTransition {
                                state_transition_resources: ReadyToWake,
                                system_apply_func: system_state_apply::<P>,
                                erased: ErasedSystemStateQueue::new(self.5.clone()),
                                waker: cx.waker().clone(),
                            },
                        ) {
                            Ok(_) => {}
                            // This should never panic because we never `close` our concurrent queues and
                            // the concurrent queue here is unbounded.
                            Err(_) => unreachable!(),
                        }
                    }
                    FutureState::Uninitialized => {
                        match global_wake_registry.0.try_send(
                            &self.3,
                            EcsTaskStateTransition {
                                state_transition_resources: Uninitialized {
                                    system_init_func: system_state_init::<P>,
                                },
                                system_apply_func: system_state_apply::<P>,
                                erased: ErasedSystemStateQueue::new(self.5.clone()),
                                waker: cx.waker().clone(),
                            },
                        ) {
                            Ok(_) => {}
                            // This should never panic because we never `close` our concurrent queues and
                            // the concurrent queue here is unbounded.
                            Err(_) => unreachable!(),
                        }
                    }
                }
                self.4.signal();
                Poll::Pending
            }
        }
    }
}
