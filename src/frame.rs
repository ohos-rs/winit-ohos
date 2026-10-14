//! XComponent frame callbacks drive redraws. Each window registers its native
//! callback only while it has frame demand and unregisters after a short idle
//! period, so an idle app receives no per-vsync wakeups.
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

    /// Records a native frame callback for this window.
    pub(crate) fn on_callback(&self) {
        let pending = {
            let mut state = self.lock();
            state.fallback_at = None;
            state.demand.on_callback()
        };
        if pending.is_some() {
            self.mark_sync();
        }
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
        let delivery = match change {
            DeliveryChange::Register => FrameInputDelivery::Continuous,
            DeliveryChange::Unregister => FrameInputDelivery::OnDemand,
        };
        let result = app.set_frame_input_delivery_for(self.id, delivery);
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
