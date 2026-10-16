use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
};

const EVENT: u8 = 1;
#[cfg(feature = "native-vsync")]
const FRAME: u8 = 2;

#[derive(Default)]
pub(crate) struct MainWake {
    requested: AtomicU8,
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl std::fmt::Debug for MainWake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainWake").finish_non_exhaustive()
    }
}

impl MainWake {
    pub(crate) fn set_waker(&self, waker: impl Fn() + Send + Sync + 'static) {
        *self.waker.lock().unwrap() = Some(Arc::new(waker));
    }

    pub(crate) fn notify(&self) {
        self.notify_for(EVENT);
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn notify_frame(&self) {
        self.notify_for(FRAME);
    }

    fn notify_for(&self, reason: u8) {
        if self.requested.fetch_or(reason, Ordering::AcqRel) != 0 {
            return;
        }
        let waker = self.waker.lock().unwrap().clone();
        if let Some(waker) = waker {
            waker();
        } else {
            self.requested.store(0, Ordering::Release);
        }
    }

    /// Whether only native frames caused this wake. Event work coalesced with a
    /// frame must still run even when that frame has no redraw demand.
    pub(crate) fn begin_turn(&self) -> bool {
        let requested = self.requested.swap(0, Ordering::AcqRel);
        #[cfg(feature = "native-vsync")]
        return requested == FRAME;
        #[cfg(not(feature = "native-vsync"))]
        {
            let _ = requested;
            false
        }
    }
}
