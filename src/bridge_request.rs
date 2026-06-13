use crate::plugin::StrongAsyncWorld;
use bevy_ecs::prelude::{IntoSystemSet, SystemSet, World};
use bevy_ecs::schedule::InternedSystemSet;

/// Drives the queued bridge work for `SyncPoint`.
///
/// Every queued bridge request is guaranteed to be *woken*. That wake guarantees the corresponding
/// async future gets a chance to poll.
/// It does *not* however guarantee the poll will finish its ECS work, because that
/// poll may still fail to finish it's work for a *variety* of reasons, i.e. it is unable to acquire
/// the typed `SystemState` lock and returns `Poll::Pending`.
///
/// For [`bevy_tasks::TaskPool::spawn_local`] we *are* actually guaranteed that the poll will finish
/// it's ECS work, because it's single threaded, so you can use `spawn_local` if you want
/// determinism.
///
/// We tick queued work multiple times for two reasons. The first is that serial `.await` calls
/// should try to all be completed within the same `SyncPoint` such as
/// ```rust,ignore
/// let health = task_1.run(|health: Single<&Health, With<Player>>| {
///     health.0
/// }).await;
/// if health == 0 {
///     return;
/// }
/// task_1.run(|commands: Commands| {
///     commands.trigger(PlayerDoesAttack);
/// }).await;
/// ```
/// The second reason is spoken of prior. Poll may fail to finish for a variety of reasons and
/// should be given several chances before giving up.
pub fn async_world_sync_point<SyncPoint: 'static>(world: &mut World) {
    // Derive the stable interned system-set key used to look up requests queued
    // for this exact sync point type.
    let sync_point = async_world_sync_point::<SyncPoint>
        .into_system_set()
        .intern();
    let async_world = world.get_resource::<StrongAsyncWorld>().unwrap().clone();

    // This ticks a single sync point, requesting the poll of all tasks in that sync point.
    // None of the tasks are guaranteed to actually return `Poll::Ready`, but all are guaranteed to
    // at least do a `Poll::Pending`
    //
    // The flow of logic is the following:
    // 1. We first drain the queue for our `SyncPoint`.
    // 2. Expose our `World` through `world_scope`.
    // 3. Wake all our `BridgeFuture`s. Each future applies its own `SystemState` back into the
    //    `World` (things like `Commands`) while it has world access.
    let this = &async_world.0;
    let mut queued_requests = bevy_platform::prelude::vec![];
    while let Ok(queued_task_bridge) = this.bridge_requests.get_or_create(&sync_point).pop() {
        queued_requests.push(queued_task_bridge);
    }
    if queued_requests.is_empty() {
        return;
    }
    this.world_scope
        .scope(world, || wake_requests_and_wait(queued_requests));
}

#[derive(Default)]
pub(crate) struct AsyncWorldInner {
    pub(crate) bridge_requests:
        keyed_concurrent_queue::KeyedQueues<InternedSystemSet, BridgeRequest>,
    pub(crate) world_scope: scoped_static_storage::ScopedStatic<World>,
}

/// We need to notify all our Wakers that have queued that we've dropped so they can error
impl Drop for AsyncWorldInner {
    fn drop(&mut self) {
        for bridge_requests in self.bridge_requests.inner().read().unwrap().values() {
            while let Ok(request) = bridge_requests.pop() {
                request.waker.wake();
            }
        }
    }
}


/// A queued access request bridging an async task into ECS.
pub(crate) struct BridgeRequest {
    /// Waker for the async future (`crate::bridge_future::BridgeFuture`) that wants ECS access.
    /// When the `SyncPoint` is driven, this waker is fired so the future can
    /// poll while `world_scope` exposes the current `World`.
    pub(crate) waker: core::task::Waker,
    /// Our custom primitive that lets us wait until all the futures have tried to run before
    /// continuing.
    pub(crate) wake_waiter: crate::wake_signal::WakeWaiter,
}

#[inline]
fn wake_requests_and_wait(queued_requests: bevy_platform::prelude::Vec<BridgeRequest>) {
    // Because currently we do not run non-conflicting system param requests in parallel (this is
    // follow up work) we can just queue and wait for each async task in order.
    for BridgeRequest { waker, wake_waiter } in queued_requests {
        wake_waiter.notify();
        waker.wake();

        #[cfg(feature = "bevy_tasks")]
        bevy_tasks::cfg::web! {
            if {} else {
                bevy_tasks::tick_global_task_pools_on_main_thread();
            }
        }

        wake_waiter.wait();
    }
}
