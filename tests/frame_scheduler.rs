//! Host tests of the production scheduler with controllable Ability/NDK boundaries.
//! See README.md for the standalone rustc command; no OHOS libraries are linked.
#![cfg(all(feature = "native-vsync", not(target_env = "ohos")))]

extern crate self as ohos_vsync_binding;
extern crate self as openharmony_ability;
extern crate self as tracing;

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { let _ = format_args!($($arg)*); };
}

#[allow(dead_code)]
#[path = "../src/frame.rs"]
mod frame;
#[path = "../src/frame_demand.rs"]
mod frame_demand;
#[path = "../src/main_wake.rs"]
mod main_wake;
#[path = "../src/native_frames.rs"]
mod native_frames;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameInputDelivery {
    #[default]
    Continuous,
    OnDemand,
}

#[derive(Default)]
struct AppState {
    deliveries: HashMap<i64, FrameInputDelivery>,
    fail: Option<FrameInputDelivery>,
}

#[derive(Clone, Default)]
pub struct OpenHarmonyApp(Arc<Mutex<AppState>>);

impl OpenHarmonyApp {
    pub fn set_frame_input_delivery_for(
        &self,
        id: i64,
        delivery: FrameInputDelivery,
    ) -> Result<(), &'static str> {
        let mut state = self.0.lock().unwrap();
        if state.fail == Some(delivery) {
            return Err("delivery failed");
        }
        state.deliveries.insert(id, delivery);
        Ok(())
    }

    pub fn refresh_rate(&self) -> u32 {
        120
    }

    pub fn create_waker(&self) -> Waker {
        Waker
    }

    fn delivery(&self, id: i64) -> FrameInputDelivery {
        self.0.lock().unwrap().deliveries[&id]
    }
}

pub struct Waker;

impl Waker {
    pub fn wake(&self) {}
}

#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OH_NativeVSync_ExpectedRateRange {
    pub min: i32,
    pub max: i32,
    pub expected: i32,
}

type Callback = Box<dyn FnMut(i64) + Send>;

#[derive(Default)]
struct Receiver {
    callbacks: VecDeque<Callback>,
    rates: Vec<OH_NativeVSync_ExpectedRateRange>,
    request_result: i32,
    destroyed: bool,
}

#[derive(Default)]
struct NativeRuntime {
    fail_create: bool,
    rate_result: i32,
    request_result: i32,
    receivers: Vec<Arc<Mutex<Receiver>>>,
}

thread_local! {
    static NATIVE: RefCell<NativeRuntime> = RefCell::default();
}

pub struct Vsync(Arc<Mutex<Receiver>>);

impl Vsync {
    pub fn try_new(_name: &str) -> Option<Self> {
        NATIVE.with_borrow_mut(|runtime| {
            if runtime.fail_create {
                return None;
            }
            let receiver = Arc::new(Mutex::new(Receiver {
                request_result: runtime.request_result,
                ..Default::default()
            }));
            runtime.receivers.push(receiver.clone());
            Some(Self(receiver))
        })
    }

    pub fn request_frame_once(&self, callback: impl FnMut(i64) + Send + 'static) -> i32 {
        let mut receiver = self.0.lock().unwrap();
        let result = receiver.request_result;
        if result == 0 {
            receiver.callbacks.push_back(Box::new(callback));
        }
        result
    }

    pub fn set_expected_frame_rate_range(&self, range: OH_NativeVSync_ExpectedRateRange) -> i32 {
        self.0.lock().unwrap().rates.push(range);
        NATIVE.with_borrow(|runtime| runtime.rate_result)
    }
}

impl Drop for Vsync {
    fn drop(&mut self) {
        // Retain callbacks so tests can simulate one racing with receiver teardown.
        self.0.lock().unwrap().destroyed = true;
    }
}

fn receiver(index: usize) -> Arc<Mutex<Receiver>> {
    NATIVE.with_borrow(|runtime| runtime.receivers[index].clone())
}

fn callback(index: usize) -> Callback {
    receiver(index)
        .lock()
        .unwrap()
        .callbacks
        .pop_front()
        .unwrap()
}

fn flush(frames: &frame::FrameScheduler, app: &OpenHarmonyApp) {
    let _turn = frame::enter_turn(frame::Turn::Event);
    frames.flush(app, Instant::now());
}

fn tick(frames: &frame::FrameScheduler, index: usize) {
    callback(index)(0);
    let _turn = frame::enter_turn(frame::Turn::Frame);
    assert!(frames.take_native_callback());
}

fn active() -> (
    frame::FrameScheduler,
    OpenHarmonyApp,
    Arc<main_wake::MainWake>,
) {
    let app = OpenHarmonyApp::default();
    let wake = Arc::new(main_wake::MainWake::default());
    wake.set_waker(|| {});
    let frames = frame::FrameScheduler::new(0, wake.clone());
    frames.sync(true, true);
    flush(&frames, &app);
    wake.begin_turn();
    (frames, app, wake)
}

#[test]
fn native_callback_wakes_ui_and_rearms_once() {
    let (frames, app, wake) = active();
    assert_eq!(app.delivery(0), FrameInputDelivery::OnDemand);
    assert!(
        !frames.on_callback(),
        "late XComponent ticks must be ignored"
    );
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    wake.set_waker(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    // Move the native callback itself to a worker, as the real binding can do.
    let mut native = callback(0);
    std::thread::spawn(move || native(0)).join().unwrap();
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(wake.begin_turn());
    assert!(frames.native_callback_pending());
    assert!(frames.take_native_callback());
    assert!(!frames.take_native_callback());
    assert_eq!(receiver(0).lock().unwrap().callbacks.len(), 1);
    assert_eq!(receiver(0).lock().unwrap().rates[0].expected, 120);
}

#[test]
fn idle_receiver_stops_and_new_demand_restarts_it() {
    let (frames, app, _) = active();
    for _ in 0..=frame_demand::IDLE_CALLBACK_THRESHOLD {
        tick(&frames, 0);
        flush(&frames, &app);
    }
    let native = receiver(0);
    let native = native.lock().unwrap();
    assert!(native.destroyed);
    assert_eq!(native.rates.last().unwrap().expected, 0);
    drop(native);
    frames.request();
    flush(&frames, &app);
    assert!(!receiver(1).lock().unwrap().destroyed);
    assert!(!frames.native_callback_pending());
    callback(0)(0);
    assert!(
        !frames.native_callback_pending(),
        "old receiver must stay isolated"
    );
    tick(&frames, 1);
}

#[test]
fn demand_during_threshold_draw_keeps_receiver_alive() {
    let (frames, app, _) = active();
    for _ in 0..=frame_demand::IDLE_CALLBACK_THRESHOLD {
        tick(&frames, 0);
    }
    frames.request();
    flush(&frames, &app);
    assert!(!receiver(0).lock().unwrap().destroyed);
    tick(&frames, 0);
}

#[test]
fn hidden_window_discards_pending_frame_and_restores_delivery() {
    let (frames, app, _) = active();
    callback(0)(0);
    frames.sync(false, true);
    assert!(!frames.take_native_callback());
    flush(&frames, &app);
    assert!(receiver(0).lock().unwrap().destroyed);
    frames.sync(true, true);
    flush(&frames, &app);
    assert!(!frames.native_callback_pending());
    tick(&frames, 1);
}

#[test]
fn surface_recreation_rejects_in_flight_callback() {
    let (frames, app, wake) = active();
    let mut old_callback = callback(0);
    frames.sync(false, false);
    frames.sync(true, true);
    flush(&frames, &app);
    wake.begin_turn();
    old_callback(0);
    assert!(!wake.begin_turn());
    assert!(!frames.native_callback_pending());
    tick(&frames, 1);
}

#[test]
fn creation_failure_uses_xcomponent_until_surface_recreation() {
    NATIVE.with_borrow_mut(|runtime| runtime.fail_create = true);
    let (frames, app, _) = active();
    assert_eq!(app.delivery(0), FrameInputDelivery::Continuous);
    assert!(frames.on_callback());
    NATIVE.with_borrow_mut(|runtime| runtime.fail_create = false);
    frames.sync(false, false);
    frames.sync(true, true);
    flush(&frames, &app);
    assert_eq!(app.delivery(0), FrameInputDelivery::OnDemand);
    assert!(!frames.on_callback());
    tick(&frames, 0);
}

#[test]
fn frame_rate_failure_releases_receiver_and_vote() {
    NATIVE.with_borrow_mut(|runtime| runtime.rate_result = -1);
    let (_, app, _) = active();
    assert_eq!(app.delivery(0), FrameInputDelivery::Continuous);
    let native = receiver(0);
    let native = native.lock().unwrap();
    assert!(native.destroyed);
    assert_eq!(native.rates.last().unwrap().expected, 0);
}

#[test]
fn initial_request_failure_uses_xcomponent() {
    NATIVE.with_borrow_mut(|runtime| runtime.request_result = -1);
    let (frames, app, _) = active();
    assert_eq!(app.delivery(0), FrameInputDelivery::Continuous);
    assert!(receiver(0).lock().unwrap().destroyed);
    assert!(frames.on_callback());
}

#[test]
fn request_chain_failure_keeps_current_tick_and_switches_source() {
    let (frames, app, _) = active();
    receiver(0).lock().unwrap().request_result = -1;
    tick(&frames, 0);
    assert!(receiver(0).lock().unwrap().destroyed);
    flush(&frames, &app);
    assert_eq!(app.delivery(0), FrameInputDelivery::Continuous);
    assert!(frames.on_callback());
}

#[test]
fn failure_to_disable_xcomponent_never_starts_native_receiver() {
    let app = OpenHarmonyApp::default();
    app.0.lock().unwrap().fail = Some(FrameInputDelivery::OnDemand);
    let frames = frame::FrameScheduler::new(0, Arc::default());
    frames.sync(true, true);
    flush(&frames, &app);
    assert_eq!(app.delivery(0), FrameInputDelivery::Continuous);
    NATIVE.with_borrow(|runtime| assert!(runtime.receivers.is_empty()));
    assert!(frames.on_callback());
}

#[test]
fn both_sources_failing_schedule_one_timer_draw() {
    NATIVE.with_borrow_mut(|runtime| runtime.fail_create = true);
    let app = OpenHarmonyApp::default();
    app.0.lock().unwrap().fail = Some(FrameInputDelivery::Continuous);
    let frames = frame::FrameScheduler::new(0, Arc::default());
    frames.sync(true, true);
    flush(&frames, &app);
    let deadline = frames.fallback_deadline().unwrap();
    assert!(!frames.begin_fallback(deadline - Duration::from_millis(1)));
    assert!(frames.begin_fallback(deadline));
    assert!(!frames.begin_fallback(deadline));
    assert_eq!(frames.fallback_deadline(), None);
}

#[test]
fn windows_have_independent_receivers() {
    let (main, app, wake) = active();
    let child = frame::FrameScheduler::new(1, wake);
    child.sync(true, true);
    flush(&child, &app);
    main.sync(false, true);
    flush(&main, &app);
    assert!(receiver(0).lock().unwrap().destroyed);
    assert!(!receiver(1).lock().unwrap().destroyed);
    tick(&child, 1);
}

#[test]
fn event_work_coalesced_with_native_wake_is_preserved() {
    let wake = main_wake::MainWake::default();
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    wake.set_waker(move || {
        count.fetch_add(1, Ordering::Relaxed);
    });
    wake.notify_frame();
    wake.notify();
    assert_eq!(wakes.load(Ordering::Relaxed), 1);
    assert!(
        !wake.begin_turn(),
        "event work must bypass the idle frame fast path"
    );
    wake.notify();
    wake.notify_frame();
    assert_eq!(wakes.load(Ordering::Relaxed), 2);
    assert!(!wake.begin_turn());
    wake.notify_frame();
    assert!(wake.begin_turn());
    assert!(!wake.begin_turn());
}
