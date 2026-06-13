use bevy_platform::sync::Arc;
use bevy_platform::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "std")]
use bevy_platform::sync::Mutex;

struct WakeInner {
    notified: AtomicBool,
    #[cfg(feature = "std")]
    acknowledged: (Mutex<bool>, std::sync::Condvar),
}

/// [`WakeSignaler`] is a custom signaling primitive used in order to fulfill our specific requirements for
/// our async bridge. We need to wait at the sync point, after waking all the futures and only when
/// all the futures have had a chance to run we stop waiting.
/// We need this signaling to occur also if the future is dropped, or if the future panics
/// so we implement the signaling *on* the Drop implementation.
/// This also makes replacing the wake signal automatically drop and signal the previous one.
pub(crate) struct WakeSignaler(Arc<WakeInner>);
/// Counterpart to the [`WakeSignaler`], the [`WakeWaiter`] waits for the [`WakeSignaler`] to drop and notify.
pub(crate) struct WakeWaiter(Arc<WakeInner>);

#[inline]
pub(crate) fn pair() -> (WakeSignaler, WakeWaiter) {
    let inner = Arc::new(WakeInner {
        notified: AtomicBool::new(false),
        #[cfg(feature = "std")]
        acknowledged: (Mutex::new(false), std::sync::Condvar::new()),
    });
    (WakeSignaler(inner.clone()), WakeWaiter(inner))
}

impl WakeSignaler {
    #[inline]
    pub(crate) fn was_notified(&self) -> bool {
        self.0.notified.load(Ordering::Acquire)
    }
}

impl WakeWaiter {
    #[inline]
    pub(crate) fn notify(&self) {
        self.0.notified.store(true, Ordering::Release);
    }

    /// Waits until another cloned instance of [`WakeSignaler`] has been dropped.
    /// If any cloned instance of [`WakeSignaler`] is dropped then this wait stops waiting.
    #[inline]
    pub(crate) fn wait(&self) {
        #[cfg(feature = "std")]
        {
            let (lock, cv) = &self.0.acknowledged;
            let mut signaled = lock.lock().unwrap();
            while !*signaled {
                signaled = cv.wait(signaled).unwrap();
            }
        }
        // No-op on no_std, since we are only using local futures we should tick them
        // prior to reaching this point.
    }
}

impl Drop for WakeSignaler {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "std")]
        {
            let (lock, cv) = &self.0.acknowledged;
            *lock.lock().unwrap() = true;
            cv.notify_one();
        }
    }
}
