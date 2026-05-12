use crate::async_ui::{AsyncUi, Ctx};
use crate::bridge_request::BridgeRequest;
use crate::wake_signal::WakeSignaler;
use crate::{AsyncWorld, BridgeError, bridge_request, cached_stuff, wake_signal};
use bevy_ecs::prelude::{IntoSystemSet, SystemSet};
use bevy_ecs::schedule::InternedSystemSet;
use bevy_ecs::system::{SystemParam, SystemParamItem, SystemParamValidationError};
use bevy_ecs::world::World;
use bevy_platform::sync::atomic::Ordering;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub trait SystemParamFunction<Marker> {
    type Out;
    type Param: SystemParam + 'static;
    fn run(self, param_value: SystemParamItem<Self::Param>) -> Self::Out;
}

pub struct FunctionSystem<Marker, Out, F>
where
    F: SystemParamFunction<Marker>,
{
    func: Option<F>,
    marker: PhantomData<(Marker, Out)>,
}

impl<Out, Func, F0: SystemParam + 'static> SystemParamFunction<fn(F0) -> Out> for Func
where
    Func: FnOnce(F0) -> Out + FnOnce(SystemParamItem<F0>) -> Out,
    Out: 'static,
{
    type Out = Out;
    type Param = (F0,);

    #[inline]
    fn run(self, param_value: SystemParamItem<Self::Param>) -> Self::Out {
        let (f0,) = param_value;
        fn call_inner<Out, F0>(f: impl FnOnce(F0) -> Out, F0: F0) -> Out {
            f(F0)
        }
        call_inner(self, f0)
    }
}

pub trait System {
    type Out;
    fn run(&mut self, world: &mut World) -> Option<Result<Self::Out, SystemParamValidationError>>;
}

impl<Marker, Out, F> System for FunctionSystem<Marker, Out, F>
where
    Marker: 'static,
    Out: 'static,
    F: SystemParamFunction<Marker, Out = Out>,
{
    type Out = Out;
    #[inline]
    fn run(&mut self, world: &mut World) -> Option<Result<Self::Out, SystemParamValidationError>> {
        let state = world
            .resource::<Ctx>()
            .cached_state::<F::Param>()
            .system_state;
        let mut state = state.try_lock::<F::Param>(world)?;
        let params = match state.get_mut(world) {
            Ok(params) => params,
            Err(e) => return Some(Err(e)),
        };
        let out = self.func.take().unwrap().run(params);
        state.apply(world);
        Some(Ok(out))
    }
}

pub trait IntoSystem<Out, Marker>: Sized {
    type System: System<Out = Out>;

    fn into_system(this: Self) -> Self::System;
}

impl<T: System> IntoSystem<T::Out, ()> for T {
    type System = T;

    fn into_system(this: Self) -> Self::System {
        this
    }
}

pub struct IsFunctionSystem;
impl<Marker, Out, F> IntoSystem<Out, (IsFunctionSystem, Marker)> for F
where
    Marker: 'static,
    Out: 'static,
    F: SystemParamFunction<Marker, Out = Out>,
{
    type System = FunctionSystem<Marker, Out, F>;

    fn into_system(this: Self) -> Self::System {
        FunctionSystem {
            func: Some(this),
            marker: PhantomData,
        }
    }
}
impl Ctx {
    pub async fn bridge<M: 'static, Out: 'static, F: IntoSystem<Out, M>>(&self, func: F) -> Out {
        CachedBridgeFuture::<<F as cached_stuff::IntoSystem<Out, M>>::System, Out, M> {
            _p: PhantomData,
            system_set: bridge_request::async_world_sync_point::<AsyncUi>
                .into_system_set()
                .intern(),
            bridge_fn: IntoSystem::into_system(func),
            wake_signal: None,
            world: self.async_world.clone(),
            is_queued: Arc::new(AtomicBool::new(false)),
        }
        .await
        .unwrap()
    }
    pub async fn try_bridge<M: 'static, Out: 'static, F: IntoSystem<Out, M>>(
        &self,
        func: F,
    ) -> Result<Out, BridgeError> {
        CachedBridgeFuture::<<F as cached_stuff::IntoSystem<Out, M>>::System, Out, M> {
            _p: PhantomData,
            system_set: bridge_request::async_world_sync_point::<AsyncUi>
                .into_system_set()
                .intern(),
            bridge_fn: IntoSystem::into_system(func),
            wake_signal: None,
            world: self.async_world.clone(),
            is_queued: Arc::new(AtomicBool::new(false)),
        }
        .await
    }
}

/// Future representing a single in-flight bridging request between our async task and our `World`.
struct CachedBridgeFuture<Func, Out, M> {
    _p: PhantomData<(M, Func, Out)>,
    /// Interned system-set key identifying which sync-point queue this future
    /// should be sent to.
    system_set: InternedSystemSet,
    /// This is the pseudo-system that we try to run when we have access to `World`.
    /// This is an option just so we can take it out when we run it so we can use `FnOnce`
    /// instead of `FnMut`, so it's more flexible than true systems.
    bridge_fn: Func,
    /// Wake signal for the currently queued wake cycle, if any.
    ///
    /// The future drops this at the end of `poll` which acts as acknowledgement that the wake
    /// has been handled.
    wake_signal: Option<WakeSignaler>,
    /// Weak bridge pointer so the loss of the world becomes a clean runtime error.
    world: AsyncWorld,
    is_queued: Arc<AtomicBool>,
}

impl<Func, Out, M> Unpin for CachedBridgeFuture<Func, Out, M> {}

impl<Func, Out, M> Future for CachedBridgeFuture<Func, Out, M>
where
    M: 'static,
    Func: System<Out = Out>,
{
    type Output = Result<Out, BridgeError>;

    fn poll(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        use core::task::Poll;

        // If we were previously woken by the sync-point driver, we will have a
        // `WakeSignaler` stored here.
        //
        // Dropping that signal at the end of this poll acts as the
        // acknowledgement that yes, this wake was observed and this task has
        // attempted its run, you may release the waiting on the other side.
        let _drop_at_end_of_scope = self.wake_signal.take();

        // Try to gain a strong reference to the bridge. If this fails, the world is gone,
        // so further access is impossible.
        let Some(strong_world) = self.world.0.upgrade() else {
            return Poll::Ready(Err(BridgeError::WorldDropped));
        };
        match if self.is_queued.load(Ordering::Relaxed) {
            strong_world
                .world_scope
                .try_with(|world| {
                    let Self {
                        ref mut bridge_fn, ..
                    } = *self;
                    // Attempt to acquire the typed `SystemState<P>`.
                    //
                    // We deliberately use `try_lock` rather than blocking. If
                    // another bridge request is currently using the same system
                    // state, we simply yield and let the sync-point driver try again
                    // on a later internal tick.

                    match bridge_fn.run(world) {
                        None => Poll::Pending,
                        Some(awa) => {
                            Poll::Ready(awa.map_err(|e| BridgeError::SystemParamValidation(e)))
                        }
                    }
                })
                .ok()
        } else {
            None
        } {
            Some(out) => out,
            None => {
                // No world is currently exposed. That means we are being polled
                // outside the `async_world_sync_point`, so we cannot access ECS yet.
                //
                // Instead, enqueue ourselves to be revisited when the matching
                // sync-point system runs.
                let (wake_signal, wake_waiter) = wake_signal::pair();
                // Store the wake_signal locally so dropping it at the end of the next
                // poll acknowledges the wake.
                self.wake_signal.replace(wake_signal);
                // Queue the request under this future's target sync point.
                //
                // The queued payload carries the following!
                // 1. The task's waker, so the sync-point driver can wake it.
                // 2. The wake handshake signal, so the driver can wait until the wake has actually
                // been processed.
                // 3. An initialization hint for the typed `SystemState`.
                // 4. The erased `SystemState` storage itself.
                strong_world
                    .bridge_requests
                    .try_send(
                        &self.system_set,
                        BridgeRequest {
                            waker: cx.waker().clone(),
                            wake_waiter,
                            system_state: None,
                            is_queued: self.is_queued.clone(),
                        },
                    )
                    .ok()
                    .unwrap();
                Poll::Pending
            }
        }
    }
}
