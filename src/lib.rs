//! The objective here is to coordinate two participants that want to share World access:
//!
//! - The main Bevy schedule
//! - Futures and async tasks running on other threads
//!
//! This is done through the bridge primitive introduced in this crate
//!
//!
//! Invariants of this crate:
//!
//! - Normal rust safety invariants for &mut World (aliasing)
//! - At most one future has world access at a time
//! - Futures only access the world while the scoped pointer (managed by the bridge driver) is live
//! - `SystemState` is always initialized before use
//! - Deferred ops are applied by each future itself while it holds world access
//! - The driver can't deadlock
//! - All futures that want world access can eventually complete (assuming fair scheduling by the async runtime)
//! - If the world is dropped, futures don't leak and eventually finish (in an error state)
//!
//!
//! The protocol:
//!
//! Futures (tasks on worker threads)
//! - enqueue requests (create signal guard clones: one kept, one sent)
//!
//! - Driver([`async_world_sync_point`]) (exclusive system, world-owning thread)
//!   1. Drain request queue for this sync point
//!   2. Publish World pointer (via `scoped_static_storage`). Future access scope begins
//!   3. Wake all drained futures
//!
//!  -> Futures race for locks (non-blocking)
//!
//!  -> Success: acquire both locks, do work, complete
//!
//!  -> Failure: signal driver (Drop signal guard), re-enqueue later
//!
//!  -> Direct access: non-queued future polled during scope,
//!  bypasses queue, acquires locks, completes (no signal)
//!
//!  -> Futures apply their own deferred ops from `SystemState` while they hold world access
//!   4. Wait for all signal guards to drop + scope mutex released
//!   5. Unpublish pointer, scope ends.
//!   6. Schedule resumes (normal systems run)
//!
//!
//! Dual locking:
//!
//! The published World pointer lock is managed by the `ScopedStatic` primitive in `scoped_static_storage` (only one future can lock this at a time)
//! `SystemState` locks are managed by the `SystemStateCell` primitive of this crate (futures using different `SystemState` types can work in parallel)
//!
//!
//! Preventing driver deadlocks when futures panic:
//!
//! If a future panics while holding locks, rust's panic unwinding drops destructors in reverse scope order
//! - First, the `SystemState` `MutexGuard` drops (releasing the lock)
//! - Second, the World pointer's scope `MutexGuard` drops (releasing the lock)
//! - Finally, the guard signal constructed by the future during `poll()` drops, and the driver is notified
//!
//! How futures can fail cleanly:
//!
//! If the [`AsyncWorld`] cannot be reached ([`bevy_platform::sync::Weak::upgrade`] fails during `poll()`), the world has been dropped and the future cannot complete.
//!
//! If `SystemState`s are invalid, they can't be used and the future cannot complete
//!
//! Regardless, the future returns Ready(Err) and completes permanently
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://!bevy.org/assets/icon.png",
    html_favicon_url = "https://!bevy.org/assets/icon.png"
)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod bridge_future;
mod bridge_request;
mod plugin;
mod system_state;
mod wake_signal;

pub use crate::bridge_future::{
    AsyncExclusiveSystemParamFunction, AsyncSystemParamFunction, AsyncSystemState, BridgeError,
    BridgeFunction, BridgeFuture, IsExclusiveBridgeFunction, IsParamBridgeFunction,
};
pub use crate::bridge_request::async_world_sync_point;
pub use crate::plugin::{AsyncPlugin, AsyncWorld};

/// The async prelude.
///
/// This includes the most common types in this crate, re-exported for your convenience.
pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        AsyncPlugin, AsyncSystemState, AsyncWorld, BridgeError, async_world_sync_point,
    };
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use bevy_app::ScheduleRunnerPlugin;
    use bevy_app::prelude::*;
    use bevy_ecs::prelude::*;
    use bevy_platform::sync::atomic::AtomicBool;
    use bevy_platform::sync::atomic::Ordering;
    use bevy_tasks::AsyncComputeTaskPool;

    /// This tests that if a world is dropped we return an error from attempting to run it and
    /// that everything cleans up nicely
    /// Because of a quirk of how bevy's task pools work we have to always have at least one
    /// active world for anything to progress on them.
    /// That's what `other_app` is for.
    #[test]
    fn dropped_world() {
        struct MySyncPoint;
        static WORLD_WAS_DROPPED: AtomicBool = AtomicBool::new(false);
        let mut other_app = App::new();
        other_app.add_plugins((TaskPoolPlugin::default(), ScheduleRunnerPlugin::default()));
        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(Startup, move |world: Res<AsyncWorld>| {
            let world = world.clone();
            AsyncComputeTaskPool::get()
                .spawn(async move {
                    let system_state = world.system_state::<Commands>();
                    match system_state
                        .bridge(MySyncPoint, |mut commands: Commands| {
                            commands.spawn_empty();
                        })
                        .await
                    {
                        Err(BridgeError::WorldDropped) => {
                            WORLD_WAS_DROPPED.store(true, Ordering::Relaxed);
                        }
                        _ => unreachable!("World should have Dropped"),
                    }
                })
                .detach();
        });
        app.update();
        drop(app);
        other_app.update();
        assert!(WORLD_WAS_DROPPED.load(Ordering::Relaxed));
    }

    #[test]
    fn invalid_parameters() {
        struct MySyncPoint;
        static FAILED_VALIDATION: AtomicBool = AtomicBool::new(false);

        #[derive(Resource)]
        struct MyResource;

        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(Update, async_world_sync_point::<MySyncPoint>);

        app.add_systems(Startup, move |world: Res<AsyncWorld>| {
            let world = world.clone();
            AsyncComputeTaskPool::get()
                .spawn(async move {
                    match world
                        .bridge(MySyncPoint, |_: Res<MyResource>| unreachable!())
                        .await
                    {
                        Err(BridgeError::SystemParamValidation(_)) => {
                            FAILED_VALIDATION.store(true, Ordering::Relaxed);
                        }
                        _ => unreachable!("Parameter validation should have failed"),
                    }
                })
                .detach();
        });

        app.update();

        assert!(FAILED_VALIDATION.load(Ordering::Relaxed));
    }

    #[test]
    fn spurious_poll_is_ignored() {
        use core::pin::Pin;
        use core::task::{Context, Poll};
        use std::time::{Duration, Instant};

        struct MySyncPoint;
        static ACCESS_RAN: AtomicBool = AtomicBool::new(false);
        static SPURIOUS_POLL_DONE: AtomicBool = AtomicBool::new(false);

        struct WakeOnce(bool);
        impl Future for WakeOnce {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.0 {
                    SPURIOUS_POLL_DONE.store(true, Ordering::Relaxed);
                    Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(Startup, move |world: Res<AsyncWorld>| {
            let world = world.clone();
            AsyncComputeTaskPool::get()
                .spawn(async move {
                    let bridge = world.bridge(MySyncPoint, |mut commands: Commands| {
                        commands.spawn_empty();
                    });
                    let (result, ()) = futures::future::join(bridge, WakeOnce(false)).await;
                    result.unwrap();
                    ACCESS_RAN.store(true, Ordering::Relaxed);
                })
                .detach();
        });

        app.update();

        let start = Instant::now();
        while !SPURIOUS_POLL_DONE.load(Ordering::Relaxed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the task never finished its spurious poll (did the bridge future panic?)"
            );
            std::thread::yield_now();
        }

        app.add_systems(Update, async_world_sync_point::<MySyncPoint>);
        let start = Instant::now();
        while !ACCESS_RAN.load(Ordering::Relaxed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the bridge never completed after the spurious poll"
            );
            app.update();
        }
    }

    #[test]
    fn serial_bridges_complete_in_one_sync_point() {
        use bevy_platform::sync::atomic::AtomicUsize;

        struct MySyncPoint;
        static BOTH_RAN: AtomicBool = AtomicBool::new(false);
        static SYNC_POINT_RUNS: AtomicUsize = AtomicUsize::new(0);
        // can't use option here, sucks, using sentinel value instead
        static FIRST_RAN_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
        // can't use option here, sucks, using sentinel value instead
        static SECOND_RAN_AT: AtomicUsize = AtomicUsize::new(usize::MAX);

        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(
            Update,
            (
                || {
                    SYNC_POINT_RUNS.fetch_add(1, Ordering::Relaxed);
                },
                async_world_sync_point::<MySyncPoint>,
            )
                .chain(),
        );

        app.add_systems(Startup, move |world: &mut World| {
            let world = world.resource::<AsyncWorld>().clone();
            AsyncComputeTaskPool::get()
                .spawn_local(async move {
                    world
                        .bridge(MySyncPoint, |mut commands: Commands| {
                            FIRST_RAN_AT.store(SYNC_POINT_RUNS.load(Ordering::Relaxed), Ordering::Relaxed);
                            commands.spawn_empty();
                        })
                        .await
                        .unwrap();
                    world
                        .bridge(MySyncPoint, |mut commands: Commands| {
                            SECOND_RAN_AT.store(SYNC_POINT_RUNS.load(Ordering::Relaxed), Ordering::Relaxed);
                            commands.spawn_empty();
                        })
                        .await
                        .unwrap();
                    BOTH_RAN.store(true, Ordering::Relaxed);
                })
                .detach();
        });

        for _ in 0..10 {
            app.update();
            if BOTH_RAN.load(Ordering::Relaxed) {
                break;
            }
        }
        assert!(BOTH_RAN.load(Ordering::Relaxed));
        assert_eq!(
            FIRST_RAN_AT.load(Ordering::Relaxed),
            SECOND_RAN_AT.load(Ordering::Relaxed),
            "the second bridge should have completed in the same sync point as the first"
        );
    }

    #[test]
    fn contended_genuine_wake_retries() {
        use core::pin::Pin;
        use core::task::{Context, Poll};
        use std::sync::Arc;
        use std::sync::mpsc::channel;
        use std::time::{Duration, Instant};

        struct SyncA;
        struct SyncB;

        static F1_RAN: AtomicBool = AtomicBool::new(false);
        static CONTENDER_HOLDS_LOCK: AtomicBool = AtomicBool::new(false);

        struct FlagWaker(AtomicBool);
        impl futures::task::ArcWake for FlagWaker {
            fn wake_by_ref(arc_self: &Arc<Self>) {
                arc_self.0.store(true, Ordering::Release);
            }
        }

        let (world_tx, world_rx) = channel();
        let (update_req_tx, update_req_rx) = channel::<()>();
        let (update_done_tx, update_done_rx) = channel::<()>();
        let app_thread = std::thread::spawn(move || {
            let mut app = App::new();
            app.add_plugins((
                AsyncPlugin::default(),
                ScheduleRunnerPlugin::default(),
                TaskPoolPlugin::default(),
            ));
            app.add_systems(Update, async_world_sync_point::<SyncA>);
            app.update();
            world_tx
                .send(app.world().resource::<AsyncWorld>().clone())
                .unwrap();
            while update_req_rx.recv().is_ok() {
                app.update();
                update_done_tx.send(()).unwrap();
            }
        });
        let world = world_rx.recv().unwrap();

        let mut f1 = world.bridge(SyncA, |mut commands: Commands| {
            commands.spawn_empty();
            F1_RAN.store(true, Ordering::Relaxed);
        });
        let flag = Arc::new(FlagWaker(AtomicBool::new(false)));
        let waker = futures::task::waker(flag.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut f1).poll(&mut cx).is_pending());

        let contender_world = world.clone();
        let contender = std::thread::spawn(move || {
            let mut f2 = contender_world.bridge(SyncB, |_: Commands| {
                CONTENDER_HOLDS_LOCK.store(true, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(200));
            });
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            loop {
                if let Poll::Ready(result) = Pin::new(&mut f2).poll(&mut cx) {
                    result.unwrap();
                    break;
                }
                core::hint::spin_loop();
            }
        });

        update_req_tx.send(()).unwrap();
        let start = Instant::now();
        while !CONTENDER_HOLDS_LOCK.load(Ordering::Relaxed) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the contender never got direct access to the world"
            );
            std::thread::yield_now();
        }

        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "f1 was starved, its genuine wake was consumed without retry or acknowledgement"
            );
            if !flag.0.swap(false, Ordering::AcqRel) {
                std::thread::yield_now();
                continue;
            }
            if let Poll::Ready(result) = Pin::new(&mut f1).poll(&mut cx) {
                result.unwrap();
                break;
            }
        }
        assert!(F1_RAN.load(Ordering::Relaxed));

        update_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the driver deadlocked at the sync point");
        contender.join().unwrap();
        drop(update_req_tx);
        app_thread.join().unwrap();
    }

    #[test]
    fn exclusive_world_access() {
        struct MySyncPoint;
        static ACCESS_RAN: AtomicBool = AtomicBool::new(false);

        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(Update, async_world_sync_point::<MySyncPoint>);

        app.add_systems(Startup, move |world: Res<AsyncWorld>| {
            let world = world.clone();
            AsyncComputeTaskPool::get()
                .spawn(async move {
                    world
                        .bridge(MySyncPoint, |world: &mut World| {
                            world.spawn_empty();
                            ACCESS_RAN.store(true, Ordering::Relaxed);
                        })
                        .await
                        .unwrap();
                })
                .detach();
        });

        app.update();

        assert!(ACCESS_RAN.load(Ordering::Relaxed));
    }

    #[test]
    #[cfg(not(feature = "std"))]
    fn no_std_test() {
        use crate::prelude::*;
        use bevy_app::ScheduleRunnerPlugin;
        use bevy_app::prelude::*;
        use bevy_ecs::prelude::*;
        use bevy_platform::sync::atomic::AtomicBool;
        use bevy_platform::sync::atomic::Ordering;
        use bevy_tasks::AsyncComputeTaskPool;

        struct MySyncPoint;
        static ACCESS_RAN: AtomicBool = AtomicBool::new(false);
        let mut app = App::new();
        app.add_plugins((
            AsyncPlugin::default(),
            ScheduleRunnerPlugin::default(),
            TaskPoolPlugin::default(),
        ));

        app.add_systems(Update, async_world_sync_point::<MySyncPoint>);

        app.add_systems(Startup, move |world: Res<AsyncWorld>| {
            let world = world.clone();
            AsyncComputeTaskPool::get()
                .spawn_local(async move {
                    let system_state = world.system_state::<Commands>();
                    system_state
                        .bridge(MySyncPoint, |mut commands: Commands| {
                            commands.spawn_empty();
                            ACCESS_RAN.store(true, Ordering::Relaxed);
                        })
                        .await
                        .unwrap();
                })
                .detach();
        });

        app.update();

        assert!(ACCESS_RAN.load(Ordering::Relaxed));
    }
}
