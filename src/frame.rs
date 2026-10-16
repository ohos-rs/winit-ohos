//! XComponent callbacks (or optional NativeVSync receivers) drive redraws. Each
//! window enables delivery only while it has frame demand and stops after a
//! short idle period, so an idle app receives no per-vsync wakeups.
use std::{
    cell::Cell,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use openharmony_ability::{FrameInputDelivery, OpenHarmonyApp};

use crate::{
    frame_demand::{DeliveryChange, FrameDemand},
    main_wake::MainWake,
};

#[cfg(feature = "native-vsync")]
use crate::native_frames::NativeFrames;

/// One fallback draw is scheduled when callback registration fails.
const FALLBACK_FRAME_DELAY: Duration = Duration::from_millis(16);

pub(crate) fn main_wake(app: &OpenHarmonyApp) -> Arc<MainWake> {
    static WAKE: OnceLock<Arc<MainWake>> = OnceLock::new();
    WAKE.get_or_init(|| {
        let wake = Arc::new(MainWake::default());
        let app = app.clone();
        wake.set_waker(move || app.create_waker().wake());
        wake
    })
    .clone()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turn {
    Idle,
    /// Delivery changes requested during this turn are applied when it ends.
    Event,
    /// Ability holds the native frame callback; delivery changes are deferred.
    Frame,
}

thread_local! {
    // OHOS uses emulated TLS; rustc 1.95 lints the macro-generated fallback.
    #[allow(clippy::missing_const_for_thread_local)]
    static TURN: Cell<Turn> = const { Cell::new(Turn::Idle) };
}

pub(crate) struct TurnGuard(Turn);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        TURN.with(|turn| turn.set(self.0));
    }
}

pub(crate) fn enter_turn(turn: Turn) -> TurnGuard {
    TurnGuard(TURN.with(|current| current.replace(turn)))
}

fn current_turn() -> Turn {
    TURN.with(Cell::get)
}

#[derive(Debug, Default)]
struct State {
    demand: FrameDemand,
    surface: bool,
    fallback_at: Option<Instant>,
    register_warned: bool,
    unregister_warned: bool,
    #[cfg(feature = "native-vsync")]
    native: Option<NativeFrames>,
    #[cfg(feature = "native-vsync")]
    native_failed: bool,
}

pub(crate) struct FrameScheduler {
    id: i64,
    state: Mutex<State>,
    sync_pending: AtomicBool,
    wake: Arc<MainWake>,
}

impl std::fmt::Debug for FrameScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameScheduler")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl FrameScheduler {
    pub(crate) fn new(id: i64, wake: Arc<MainWake>) -> Self {
        let scheduler = Self {
            id,
            state: Mutex::default(),
            sync_pending: AtomicBool::new(false),
            wake,
        };
        // Ability defaults to continuous callbacks; start on demand.
        scheduler.mark_sync();
        scheduler
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mark_sync(&self) {
        if !self.sync_pending.swap(true, Ordering::AcqRel) && current_turn() != Turn::Event {
            self.wake.notify();
        }
    }

    /// Tracks surface presentability. A new surface starts from ability's recorded
    /// delivery, so registration state is reset when the old one goes away.
    pub(crate) fn sync(&self, presentable: bool, surface_available: bool) {
        let pending = {
            let mut state = self.lock();
            if state.surface && !surface_available {
                state.demand.surface_destroyed();
                state.fallback_at = None;
                #[cfg(feature = "native-vsync")]
                {
                    state.native.take();
                    state.native_failed = false;
                }
            }
            state.surface = surface_available;
            state.demand.set_active(presentable);
            if !presentable {
                state.fallback_at = None;
            }
            state.demand.pending()
        };
        if pending.is_some() {
            self.mark_sync();
        }
    }

    pub(crate) fn request(&self) {
        if self.lock().demand.request().is_some() {
            self.mark_sync();
        }
    }

    /// Records an XComponent callback, ignoring it while NativeVSync is selected.
    pub(crate) fn on_callback(&self) -> bool {
        let pending = {
            let mut state = self.lock();
            #[cfg(feature = "native-vsync")]
            if !state.native_failed {
                return false;
            }
            state.fallback_at = None;
            state.demand.on_callback()
        };
        if pending.is_some() {
            self.mark_sync();
        }
        true
    }

    /// Consumes a NativeVSync signal on the UI thread and arms the next tick
    /// before drawing, so demand from the idle-threshold draw can keep it alive.
    #[cfg(feature = "native-vsync")]
    pub(crate) fn take_native_callback(&self) -> bool {
        let mut state = self.lock();
        if !state.demand.is_active()
            || !state
                .native
                .as_ref()
                .is_some_and(NativeFrames::take_callback)
        {
            return false;
        }
        state.fallback_at = None;
        state.demand.on_callback();
        if let Err(error) = state.native.as_ref().unwrap().request() {
            tracing::warn!(
                "NativeVSync stopped for window {}; falling back to XComponent callbacks: {error}",
                self.id
            );
            state.native.take();
            state.native_failed = true;
            state.demand.complete(DeliveryChange::Register, false);
            state.demand.request();
        }
        if state.demand.pending().is_some() {
            self.mark_sync();
        }
        true
    }

    #[cfg(feature = "native-vsync")]
    pub(crate) fn native_callback_pending(&self) -> bool {
        self.lock()
            .native
            .as_ref()
            .is_some_and(NativeFrames::callback_pending)
    }

    /// Applies a pending delivery change. Must not run inside a frame callback.
    pub(crate) fn flush(&self, app: &OpenHarmonyApp, now: Instant) {
        debug_assert_ne!(current_turn(), Turn::Frame);
        if !self.sync_pending.swap(false, Ordering::AcqRel) {
            return;
        }
        // Hold the demand lock across the native call so a concurrent request
        // cannot be lost between choosing and completing the change.
        let mut state = self.lock();
        let Some(change) = state.demand.pending() else {
            return;
        };
        #[cfg(feature = "native-vsync")]
        let was_native = state.native.is_some();
        #[cfg(feature = "native-vsync")]
        match change {
            DeliveryChange::Register if !state.native_failed => {
                // Release XComponent's callback before creating an independent
                // receiver. Both sources must never drive the same window.
                match app.set_frame_input_delivery_for(self.id, FrameInputDelivery::OnDemand) {
                    Ok(()) => match NativeFrames::new(app.refresh_rate(), self.wake.clone()) {
                        Ok(native) => state.native = Some(native),
                        Err(error) => {
                            tracing::warn!(
                                "Cannot start NativeVSync for window {}; falling back to \
                                 XComponent callbacks: {error}",
                                self.id
                            );
                            state.native_failed = true;
                        }
                    },
                    Err(error) => {
                        tracing::warn!(
                            "Cannot select NativeVSync for window {}; falling back to \
                             XComponent callbacks: {error}",
                            self.id
                        );
                        state.native_failed = true;
                    }
                }
            }
            DeliveryChange::Unregister => {
                state.native.take();
            }
            _ => {}
        }
        let delivery = match change {
            DeliveryChange::Register => FrameInputDelivery::Continuous,
            DeliveryChange::Unregister => FrameInputDelivery::OnDemand,
        };
        #[cfg(not(feature = "native-vsync"))]
        let result = app.set_frame_input_delivery_for(self.id, delivery);
        #[cfg(feature = "native-vsync")]
        let result =
            if state.native.is_some() || (was_native && change == DeliveryChange::Unregister) {
                // NativeVSync registration already disabled XComponent delivery;
                // dropping that receiver needs no further Ability mutation.
                Ok(())
            } else {
                app.set_frame_input_delivery_for(self.id, delivery)
            };
        state.demand.complete(change, result.is_ok());
        let Err(error) = result else {
            return;
        };
        match change {
            DeliveryChange::Register => {
                if !std::mem::replace(&mut state.register_warned, true) {
                    tracing::warn!(
                        "OHOS frame callback registration failed for window {}: {error}",
                        self.id
                    );
                }
                if state.demand.schedule_fallback() {
                    state.fallback_at = Some(now + FALLBACK_FRAME_DELAY);
                }
            }
            DeliveryChange::Unregister => {
                if !std::mem::replace(&mut state.unregister_warned, true) {
                    tracing::warn!(
                        "OHOS frame callback unregistration failed for window {}; keeping \
                         continuous delivery until the surface is recreated: {error}",
                        self.id
                    );
                }
            }
        }
    }

    pub(crate) fn fallback_deadline(&self) -> Option<Instant> {
        self.lock().fallback_at
    }

    /// Starts a due fallback draw, returning whether it should present.
    pub(crate) fn begin_fallback(&self, now: Instant) -> bool {
        let mut state = self.lock();
        if !state.fallback_at.is_some_and(|deadline| deadline <= now) {
            return false;
        }
        state.fallback_at = None;
        state.demand.begin_fallback()
    }
}
