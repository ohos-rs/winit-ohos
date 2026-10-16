use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use openharmony_ability::xcomponent::{
    Action, KeyCode as Keycode, MouseAction as OhosMouseAction, MouseButton as OhosMouseButton,
    MouseEventData, TouchEvent, TouchEventData, TouchPointData,
};
use tracing::{trace, warn};

use openharmony_ability::{
    ArkUiInputEvent, AvoidAreaType, ColorMode, Configuration, Event as MainEvent, GestureEvent,
    GesturePhase as OhosGesturePhase, ImeEvent, InputEvent, KeyboardEventData,
    KeyboardInputDelivery, OpenHarmonyApp, Rect, XComponentInputEvent,
    arkui::arkui_input_binding::{UIInputAction, UIInputEvent, UIInputSourceType, UIInputToolType},
    ime::KeyboardStatus,
};

use dpi::{PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{Cursor, CustomCursor, CustomCursorSource};
use winit_core::error::{
    CreateWindowError, CustomCursorError, EventLoopError, NotSupportedError, OsError, RequestError,
};
use winit_core::event::{
    self, ButtonSource, DeviceId, ElementState, FingerId, Force, Ime, Modifiers, MouseButton,
    MouseScrollDelta, PointerKind, PointerSource, StartCause, SurfaceSizeWriter, TouchPhase,
};
use winit_core::event_loop::pump_events::PumpStatus;
use winit_core::event_loop::register::EventLoopExtRegister;
use winit_core::event_loop::{
    ActiveEventLoop as RootActiveEventLoop, ControlFlow, DeviceEvents, EventLoopProvider,
    EventLoopProxy as CoreEventLoopProxy, EventLoopProxyProvider,
    OwnedDisplayHandle as CoreOwnedDisplayHandle,
};
use winit_core::keyboard::{ModifiersKeys, ModifiersState};
use winit_core::monitor::{Fullscreen, MonitorHandle as CoreMonitorHandle};
use winit_core::window::{
    self, CursorGrabMode, ImeCapabilities, ImePurpose, ImeRequest, ImeRequestError,
    ResizeDirection, Theme, Window as CoreWindow, WindowAttributes, WindowButtons, WindowId,
    WindowLevel, WindowType,
};

use crate::keycodes;

const GLOBAL_WINDOW: WindowId = WindowId::from_raw(0);
static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);

enum LoopEvent<'a> {
    Ability(MainEvent<'a>),
    #[cfg(feature = "native-vsync")]
    NativeFrame(WindowId),
}

#[derive(Debug)]
pub struct EventLoop {
    pub openharmony_app: OpenHarmonyApp,
    window_target: ActiveEventLoop,
    loop_running: bool,
    surface_available: bool,
    timer: crate::timer::Timer,
    resumed: bool,
    current_window: WindowId,
    input: InputState,
    input_states: HashMap<WindowId, InputState>,
    wait_started: Instant,
    fallback_deadline: Option<Instant>,
}

#[derive(Debug, Default)]
struct InputState {
    primary_pointer: Option<FingerId>,
    pressed_fingers: HashSet<i32>,
    pending_touch: Option<TouchEventData>,
    pen_metadata: Option<openharmony_ability::StylusInputData>,
    mouse_inside: bool,
    pressed_mouse_buttons: Vec<ButtonSource>,
    modifiers: ModifiersState,
    modifier_keys: ModifiersKeys,
}

#[derive(Debug)]
struct SharedWindowState {
    commands: crate::task::CommandQueue,
    ready: Mutex<Option<futures_channel::oneshot::Sender<()>>>,
    inner: Mutex<WindowState>,
    frame: crate::frame::FrameScheduler,
    geometry: crate::geometry::Geometry<GeometryTarget>,
}

#[derive(Debug, Clone, Copy)]
struct GeometryTarget {
    position: PhysicalPosition<i32>,
    size: Option<PhysicalSize<u32>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CursorKey {
    Icon(i32),
    Custom(CustomCursor),
}

#[derive(Debug, Clone, Copy)]
struct WindowDrag {
    direction: Option<ResizeDirection>,
    pointer: PhysicalPosition<f64>,
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
}

#[derive(Debug)]
struct WindowState {
    lifecycle: crate::window_lifecycle::WindowLifecycle,
    cursor_key: Option<CursorKey>,
    cursor_pending: Option<CursorKey>,
    cursor_generation: u64,
    surface_size: PhysicalSize<u32>,
    surface_position: PhysicalPosition<i32>,
    outer_size: PhysicalSize<u32>,
    position: PhysicalPosition<i32>,
    scale_factor: f64,
    focused: bool,
    visible: bool,
    theme: Option<Theme>,
    ime_capabilities: Option<ImeCapabilities>,
    title: String,
    resizable: bool,
    decorations: bool,
    enabled_buttons: WindowButtons,
    minimized: bool,
    maximized: bool,
    fullscreen: Option<Fullscreen>,
    min_size: Option<PhysicalSize<u32>>,
    max_size: Option<PhysicalSize<u32>>,
    real_window_id: Option<i32>,
    initial_attributes: Option<WindowAttributes>,
    redraw_pending: bool,
    resize_settle: crate::resize_settle::ResizeSettle,
    pointer_position: PhysicalPosition<f64>,
    dragging: Option<WindowDrag>,
    resize_increments: Option<PhysicalSize<u32>>,
    ime_edits: Vec<crate::ImeEdit>,
    /// Set once the application calls `take_ime_edits`; until then no edits are queued.
    native_ime_edits: bool,
    ime_surrounding: Option<crate::ime_text::SurroundingText>,
}

impl SharedWindowState {
    fn new(app: &OpenHarmonyApp, id: i64, visible: bool) -> Self {
        let content_rect = app.content_rect_for(id);
        let window_rect = app.window_rect_for(id);
        let surface_size = physical_size(content_rect.width, content_rect.height);
        let outer_size = physical_size(window_rect.width, window_rect.height);

        let commands = crate::task::CommandQueue::default();
        let ready = if id == 0 {
            None
        } else {
            let (ready, wait) = futures_channel::oneshot::channel();
            // Eframe may issue size/position requests as soon as create_window returns.
            // Float window creation is asynchronous; queue those requests until its
            // XComponent exists and WindowManager has registered the logical ID.
            commands.push(async move {
                let _ = wait.await;
            });
            Some(ready)
        };

        Self {
            commands,
            ready: Mutex::new(ready),
            frame: crate::frame::FrameScheduler::new(id, crate::frame::main_wake(app)),
            geometry: Default::default(),
            inner: Mutex::new(WindowState {
                lifecycle: Default::default(),
                cursor_key: None,
                cursor_pending: None,
                cursor_generation: 0,
                surface_size,
                surface_position: PhysicalPosition::new(content_rect.left, content_rect.top),
                outer_size: nonzero_size_or(outer_size, surface_size),
                position: PhysicalPosition::new(window_rect.left, window_rect.top),
                // The display API may not be ready while `#[ability]` is still initializing.
                scale_factor: 1.0,
                focused: false,
                visible,
                theme: theme(app.config().color_mode),
                ime_capabilities: None,
                title: String::new(),
                resizable: true,
                decorations: true,
                enabled_buttons: WindowButtons::all(),
                minimized: false,
                maximized: false,
                fullscreen: None,
                min_size: None,
                max_size: None,
                real_window_id: None,
                initial_attributes: None,
                redraw_pending: false,
                resize_settle: Default::default(),
                pointer_position: PhysicalPosition::default(),
                dragging: None,
                resize_increments: None,
                ime_edits: Vec::new(),
                native_ime_edits: false,
                ime_surrounding: None,
            }),
        }
    }

    fn request_redraw(&self) {
        let mut state = self.lock();
        state.redraw_pending = true;
        if state.presentable() {
            self.frame.request();
        }
    }

    fn sync_active(&self, state: &WindowState) {
        self.frame
            .sync(state.presentable(), state.lifecycle.surface_available);
        if state.presentable() && state.redraw_pending {
            self.frame.request();
        }
    }

    fn lock(&self) -> MutexGuard<'_, WindowState> {
        self.inner.lock().unwrap()
    }
}

impl WindowState {
    fn invalidate_cursor(&mut self) {
        self.cursor_key = None;
        self.cursor_pending = None;
        self.cursor_generation = self.cursor_generation.wrapping_add(1);
    }

    fn presentable(&self) -> bool {
        self.lifecycle.presentable(self.visible, self.minimized)
    }

    fn occluded(&self) -> bool {
        !self.visible || self.minimized
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlatformSpecificEventLoopAttributes {
    pub openharmony_app: Option<OpenHarmonyApp>,
}

impl EventLoop {
    pub fn new(attributes: &PlatformSpecificEventLoopAttributes) -> Result<Self, EventLoopError> {
        let openharmony_app = attributes.openharmony_app.as_ref().expect(
            "An `OpenHarmonyApp` as passed to lib is required to create an `EventLoop` on \
             OpenHarmony or HarmonyNext",
        );

        // ArkUI's post-IME stream provides the system-translated character and
        // held modifiers; raw XComponent key codes lose Shift and keyboard layouts.
        if let Err(error) =
            openharmony_app.set_keyboard_input_delivery(KeyboardInputDelivery::ArkUi)
        {
            warn!("ArkUI keyboard input unavailable; using raw key codes: {error}");
        }

        crate::plugins::register_enabled(openharmony_app).map_err(|error| {
            EventLoopError::Os(OsError::new(
                line!(),
                file!(),
                std::io::Error::other(error.reason.clone()),
            ))
        })?;

        if EVENT_LOOP_CREATED.swap(true, Ordering::AcqRel) {
            return Err(EventLoopError::RecreationAttempt);
        }

        let event_loop_proxy = Arc::new(EventLoopProxy::new(openharmony_app.clone()));
        let shared = Arc::new(SharedWindowState::new(openharmony_app, 0, true));
        shared.frame.flush(openharmony_app, Instant::now());
        let main_wake = crate::frame::main_wake(openharmony_app);
        let timer_wake = main_wake.clone();

        Ok(Self {
            timer: crate::timer::Timer::new(move || timer_wake.notify()),
            openharmony_app: openharmony_app.clone(),
            window_target: ActiveEventLoop {
                app: openharmony_app.clone(),
                main_wake,
                visible: Cell::new(true),
                control_flow: Cell::new(ControlFlow::default()),
                exit: Cell::new(false),
                event_loop_proxy,
                drag_drop: Default::default(),
                windows: Mutex::new(HashMap::from([(GLOBAL_WINDOW, shared.clone())])),
                root_claimed: Arc::new(AtomicBool::new(false)),
                shared,
            },
            loop_running: false,
            surface_available: false,
            resumed: false,
            current_window: GLOBAL_WINDOW,
            input: InputState::default(),
            input_states: HashMap::new(),
            wait_started: Instant::now(),
            fallback_deadline: None,
        })
    }

    pub fn window_target(&self) -> &dyn RootActiveEventLoop {
        &self.window_target
    }

    fn handle_input_event<A: ApplicationHandler>(&mut self, event: &InputEvent, app: &mut A) {
        if let Some(touch) = self.input.pending_touch.take() {
            // ArkUI reports mouse presses through both native touch and mouse callbacks.
            // Ability follows each native touch with metadata identifying its source.
            let mouse_emulated_touch = matches!(event,
                InputEvent::ArkUi(ArkUiInputEvent::Pointer(pointer))
                    if pointer.event_type == UIInputEvent::Touch
                        && pointer.timestamp == touch.timestamp
                        && pointer.source_type == UIInputSourceType::Mouse
            );
            self.input.pen_metadata = match event {
                InputEvent::ArkUi(ArkUiInputEvent::Pointer(pointer))
                    if pointer.timestamp == touch.timestamp =>
                {
                    pointer.stylus
                }
                _ => None,
            };
            if !mouse_emulated_touch {
                self.handle_touch_event(&touch, app);
            }
            self.input.pen_metadata = None;
        }
        match event {
            InputEvent::XComponent(event) => match event {
                XComponentInputEvent::Touch(motion_event) => {
                    let mouse_tool = motion_event
                        .touch_points
                        .iter()
                        .find(|point| point.id == motion_event.id)
                        .is_some_and(|point| {
                            point.event_tool_type
                                == openharmony_ability::xcomponent::TouchPointTool::Mouse
                        });
                    if mouse_tool {
                        trace!(
                            "Ignoring a mouse-emulated OHOS touch; the mouse callback reports it"
                        );
                    } else {
                        self.input.pending_touch = Some(motion_event.clone());
                        // ArkUI metadata normally follows in the same input dispatch. If
                        // it does not, the next frame turn delivers the touch on its own.
                        let shared = &self.window_target.shared;
                        if shared.lock().presentable() {
                            shared.frame.request();
                        } else {
                            self.window_target.main_wake.notify();
                        }
                    }
                }
                XComponentInputEvent::Mouse(mouse_event) => {
                    self.handle_mouse_event(mouse_event, app);
                }
                XComponentInputEvent::Hover(hovered) => {
                    if !hovered {
                        self.window_target.shared.lock().invalidate_cursor();
                    }
                    if !hovered && self.input.mouse_inside {
                        self.input.mouse_inside = false;
                        app.window_event(
                            &self.window_target,
                            self.current_window,
                            event::WindowEvent::PointerLeft {
                                device_id: None,
                                primary: true,
                                position: None,
                                kind: PointerKind::Mouse,
                            },
                        );
                    }
                }
                XComponentInputEvent::Key(key) => match key.action {
                    Action::Down => self.emit_key_event(
                        key.code,
                        ElementState::Pressed,
                        Some(DeviceId::from_raw(key.device_id)),
                        false,
                        false,
                        app,
                    ),
                    Action::Up => self.emit_key_event(
                        key.code,
                        ElementState::Released,
                        Some(DeviceId::from_raw(key.device_id)),
                        false,
                        false,
                        app,
                    ),
                    Action::Unknown => trace!("Ignoring an OHOS key event with unknown action"),
                },
            },
            InputEvent::ArkUi(event) => self.handle_arkui_input_event(event, app),
            InputEvent::Ime(data) => {
                match data {
                    ImeEvent::TextInputEvent(s) => {
                        app.window_event(
                            &self.window_target,
                            self.current_window,
                            event::WindowEvent::Ime(Ime::Commit(s.text.clone())),
                        );
                    }
                    ImeEvent::BackspaceEvent(count) | ImeEvent::DeleteForwardEvent(count) => {
                        self.handle_ime_deletion(
                            matches!(data, ImeEvent::BackspaceEvent(_)),
                            (*count).max(0) as usize,
                            app,
                        );
                    }
                    ImeEvent::PreviewTextEvent { text, .. } => app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::Ime(Ime::Preedit(
                            text.clone(),
                            Some((text.len(), text.len())),
                        )),
                    ),
                    ImeEvent::FinishPreviewEvent => app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::Ime(Ime::Preedit(String::new(), None)),
                    ),
                    ImeEvent::MoveCursorEvent(direction) => {
                        use openharmony_ability::ime::Direction;
                        let key = match direction {
                            Direction::Left => Some(Keycode::DpadLeft),
                            Direction::Right => Some(Keycode::DpadRight),
                            Direction::Up => Some(Keycode::DpadUp),
                            Direction::Down => Some(Keycode::DpadDown),
                            Direction::None => None,
                        };
                        if let Some(key) = key {
                            self.emit_synthetic_key(key, app);
                        }
                    }
                    ImeEvent::ExtendActionEvent(action) => {
                        use openharmony_ability::ime::Action;
                        let key = match action {
                            Action::Copy => Keycode::Copy,
                            Action::Cut => Keycode::Cut,
                            Action::Paste => Keycode::Paste,
                            Action::SelectAll => Keycode::A,
                        };
                        let select_all = matches!(action, Action::SelectAll);
                        let modifiers = self.input.modifiers;
                        if select_all {
                            self.input.modifiers.insert(ModifiersState::CONTROL);
                            app.window_event(
                                &self.window_target,
                                self.current_window,
                                event::WindowEvent::ModifiersChanged(Modifiers::new(
                                    self.input.modifiers,
                                    self.input.modifier_keys,
                                )),
                            );
                        }
                        self.emit_synthetic_key(key, app);
                        if select_all {
                            self.input.modifiers = modifiers;
                            app.window_event(
                                &self.window_target,
                                self.current_window,
                                event::WindowEvent::ModifiersChanged(Modifiers::new(
                                    self.input.modifiers,
                                    self.input.modifier_keys,
                                )),
                            );
                        }
                    }
                    ImeEvent::SelectionEvent { start, end } => {
                        let mut state = self.window_target.shared.lock();
                        if !state.native_ime_edits {
                            // winit has no portable selection event.
                            trace!("Ignoring an OHOS IME selection without a native edit consumer");
                            return;
                        }
                        state.ime_edits.push(crate::ImeEdit::Selection {
                            start: (*start).max(0) as usize,
                            end: (*end).max(0) as usize,
                        });
                        drop(state);
                        app.window_event(
                            &self.window_target,
                            self.current_window,
                            event::WindowEvent::Ime(Ime::Enabled),
                        );
                    }
                    ImeEvent::EnterEvent(_) => self.emit_synthetic_key(Keycode::Enter, app),
                    ImeEvent::ImeStatusEvent(status) => match status {
                        KeyboardStatus::Hide => {
                            let mut state = self.window_target.shared.lock();
                            state.ime_capabilities = None;
                            state.ime_surrounding = None;
                            drop(state);
                            app.window_event(
                                &self.window_target,
                                self.current_window,
                                event::WindowEvent::Ime(Ime::Disabled),
                            );
                        }
                        KeyboardStatus::Show => {
                            app.window_event(
                                &self.window_target,
                                self.current_window,
                                event::WindowEvent::Ime(Ime::Enabled),
                            );
                        }
                        KeyboardStatus::None => trace!("Ignoring an empty OHOS IME status event"),
                    },
                }
            }
        }
    }

    fn handle_arkui_input_event<A: ApplicationHandler>(
        &mut self,
        event: &ArkUiInputEvent,
        app: &mut A,
    ) {
        match event {
            ArkUiInputEvent::Pointer(pointer) => {
                // Metadata only: do not duplicate the corresponding native input.
                crate::drag_drop::set_pointer(self.current_window, pointer.pointer_id);
            }
            ArkUiInputEvent::KeyPreIme(key) => self.sync_key_modifiers(key, app),
            ArkUiInputEvent::Key(key) => {
                self.sync_key_modifiers(key, app);
                let state = match key.action {
                    Action::Down => ElementState::Pressed,
                    Action::Up => ElementState::Released,
                    Action::Unknown => return,
                };
                self.emit_key_event_with_unicode(
                    key.code,
                    state,
                    Some(DeviceId::from_raw(key.device_id)),
                    false,
                    false,
                    char::from_u32(key.unicode).filter(|ch| !ch.is_control()),
                    app,
                );
            }
            ArkUiInputEvent::Drag(drag) => {
                let offset = self.window_target.shared.lock().surface_position;
                let events = self.window_target.drag_drop.lock().unwrap().event(
                    self.current_window,
                    drag,
                    offset,
                );
                for event in events {
                    app.window_event(&self.window_target, self.current_window, event);
                }
            }
            ArkUiInputEvent::Axis(event) => {
                let mouse_wheel = event.pointer.tool_type == UIInputToolType::Mouse;
                // A wheel notch arrives as a complete Down/Move/Up sequence in one
                // frame. It is a discrete scroll, not a touchpad gesture. In egui,
                // a gesture End would clear the deltas queued in that same frame.
                if mouse_wheel && event.pointer.action != UIInputAction::Move {
                    return;
                }
                let position =
                    PhysicalPosition::new(f64::from(event.pointer.x), f64::from(event.pointer.y));
                self.ensure_mouse_entered(None, position, app);
                app.window_event(
                    &self.window_target,
                    self.current_window,
                    event::WindowEvent::PointerMoved {
                        device_id: None,
                        position,
                        primary: true,
                        source: PointerSource::Mouse,
                    },
                );
                // OHOS wheel values are degrees (15 per notch); touchpads use pixels.
                // Positive OHOS values scroll down/right, opposite winit's convention.
                let delta = if mouse_wheel {
                    MouseScrollDelta::LineDelta(
                        (-event.delta_x / 15.0) as f32,
                        (-event.delta_y / 15.0) as f32,
                    )
                } else {
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(
                        -event.delta_x,
                        -event.delta_y,
                    ))
                };
                app.window_event(
                    &self.window_target,
                    self.current_window,
                    event::WindowEvent::MouseWheel {
                        device_id: None,
                        delta,
                        phase: if mouse_wheel {
                            TouchPhase::Moved
                        } else {
                            touch_phase_from_arkui(event.pointer.action)
                        },
                    },
                );
            }
            ArkUiInputEvent::Gesture(GestureEvent::Tap(event)) => {
                if self.openharmony_app.touch_input_delivery()
                    != openharmony_ability::TouchInputDelivery::ArkUiGestures
                {
                    trace!("Ignoring OHOS Tap alongside raw touch delivery");
                    return;
                }
                let finger_id = event.pointer.pointer_id.unwrap_or(0);
                self.emit_touch_point(
                    TouchEvent::Down,
                    finger_id,
                    event.pointer.x,
                    event.pointer.y,
                    0.0,
                    0,
                    app,
                );
                self.emit_touch_point(
                    TouchEvent::Up,
                    finger_id,
                    event.pointer.x,
                    event.pointer.y,
                    0.0,
                    0,
                    app,
                );
            }
            ArkUiInputEvent::Gesture(GestureEvent::Pan(event)) => app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PanGesture {
                    device_id: None,
                    delta: PhysicalPosition::new(event.delta_x, event.delta_y),
                    phase: touch_phase_from_gesture(event.phase),
                },
            ),
            ArkUiInputEvent::Gesture(GestureEvent::Swipe(_)) => {
                // Winit has no swipe event. Ability emits the pan stream for the same gesture,
                // which preserves its motion without inventing a second synthetic event.
                trace!("Ignoring an OHOS swipe event after its pan stream");
            }
        }
    }

    fn handle_pen_event<A: ApplicationHandler>(
        &mut self,
        position: PhysicalPosition<f64>,
        action: UIInputAction,
        kind: event::TabletToolKind,
        data: event::TabletToolData,
        app: &mut A,
    ) {
        use event::TabletToolButton;
        self.update_window_drag(
            position,
            matches!(action, UIInputAction::Up | UIInputAction::Cancel),
        );
        if action == UIInputAction::Cancel {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerLeft {
                    device_id: None,
                    primary: true,
                    position: Some(position),
                    kind: PointerKind::TabletTool(kind),
                },
            );
            return;
        }
        if action == UIInputAction::Down {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerEntered {
                    device_id: None,
                    primary: true,
                    position,
                    kind: PointerKind::TabletTool(kind),
                },
            );
        }
        app.window_event(
            &self.window_target,
            self.current_window,
            event::WindowEvent::PointerMoved {
                device_id: None,
                primary: true,
                position,
                source: PointerSource::TabletTool {
                    kind,
                    data: data.clone(),
                },
            },
        );
        if matches!(
            action,
            UIInputAction::Down | UIInputAction::Up | UIInputAction::Cancel
        ) {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerButton {
                    device_id: None,
                    primary: true,
                    position,
                    state: if action == UIInputAction::Down {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                    button: ButtonSource::TabletTool {
                        kind,
                        button: TabletToolButton::Contact,
                        data,
                    },
                    is_macos_activation_click: false,
                },
            );
        }
        if matches!(action, UIInputAction::Up | UIInputAction::Cancel) {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerLeft {
                    device_id: None,
                    primary: true,
                    position: Some(position),
                    kind: PointerKind::TabletTool(kind),
                },
            );
        }
    }

    fn handle_touch_event<A: ApplicationHandler>(
        &mut self,
        motion_event: &TouchEventData,
        app: &mut A,
    ) {
        match motion_event.event_type {
            TouchEvent::Move => {
                if motion_event.touch_points.is_empty() {
                    self.emit_touch_point(
                        motion_event.event_type,
                        motion_event.id,
                        motion_event.x,
                        motion_event.y,
                        motion_event.force,
                        motion_event.device_id,
                        app,
                    );
                } else {
                    for point in &motion_event.touch_points {
                        self.emit_touch_data(motion_event, point, app);
                    }
                }
            }
            TouchEvent::Cancel => {
                if motion_event.touch_points.is_empty() {
                    self.emit_touch_point(
                        motion_event.event_type,
                        motion_event.id,
                        motion_event.x,
                        motion_event.y,
                        motion_event.force,
                        motion_event.device_id,
                        app,
                    );
                } else {
                    for point in &motion_event.touch_points {
                        self.emit_touch_data(motion_event, point, app);
                    }
                }
            }
            TouchEvent::Down | TouchEvent::Up => {
                if let Some(point) = motion_event
                    .touch_points
                    .iter()
                    .find(|point| point.id == motion_event.id)
                {
                    self.emit_touch_data(motion_event, point, app);
                } else {
                    self.emit_touch_point(
                        motion_event.event_type,
                        motion_event.id,
                        motion_event.x,
                        motion_event.y,
                        motion_event.force,
                        motion_event.device_id,
                        app,
                    );
                }
            }
            TouchEvent::Unknown => trace!("Ignoring an OHOS touch event with unknown action"),
        }
    }

    fn emit_touch_data<A: ApplicationHandler>(
        &mut self,
        event: &TouchEventData,
        point: &TouchPointData,
        app: &mut A,
    ) {
        if let Some(kind) = crate::stylus::tool_kind(point.event_tool_type)
            .or_else(|| self.input.pen_metadata.map(|_| event::TabletToolKind::Pen))
        {
            let mut data = self
                .input
                .pen_metadata
                .map(crate::stylus::tablet_data)
                .unwrap_or_default();
            data.force = point
                .force
                .is_finite()
                .then(|| Force::Normalized(normalized_force(point.force)));
            if let Some((x, y)) = point.tilt_x.zip(point.tilt_y) {
                data.tilt = Some(event::TabletToolTilt {
                    x: x.round().clamp(-90.0, 90.0) as i8,
                    y: y.round().clamp(-90.0, 90.0) as i8,
                });
            }
            let action = match event.event_type {
                TouchEvent::Down => UIInputAction::Down,
                TouchEvent::Up => UIInputAction::Up,
                TouchEvent::Cancel => UIInputAction::Cancel,
                _ => UIInputAction::Move,
            };
            self.handle_pen_event(
                PhysicalPosition::new(f64::from(point.x), f64::from(point.y)),
                action,
                kind,
                data,
                app,
            );
            return;
        }
        self.emit_touch_point(
            event.event_type,
            point.id,
            point.x,
            point.y,
            point.force,
            event.device_id,
            app,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_touch_point<A: ApplicationHandler>(
        &mut self,
        action: TouchEvent,
        raw_finger_id: i32,
        x: f32,
        y: f32,
        raw_force: f32,
        raw_device_id: i64,
        app: &mut A,
    ) {
        if let Some(pen) = self.input.pen_metadata {
            let action = match action {
                TouchEvent::Down => UIInputAction::Down,
                TouchEvent::Up => UIInputAction::Up,
                TouchEvent::Cancel => UIInputAction::Cancel,
                _ => UIInputAction::Move,
            };
            self.handle_pen_event(
                PhysicalPosition::new(f64::from(x), f64::from(y)),
                action,
                event::TabletToolKind::Pen,
                crate::stylus::tablet_data(pen),
                app,
            );
            return;
        }
        let finger_id = FingerId::from_raw(usize::try_from(raw_finger_id).unwrap_or(0));
        self.update_window_drag(
            PhysicalPosition::new(x as f64, y as f64),
            matches!(action, TouchEvent::Up | TouchEvent::Cancel),
        );
        let device_id = Some(DeviceId::from_raw(raw_device_id));
        let position = PhysicalPosition::new(x as f64, y as f64);
        let force = Some(Force::Normalized(normalized_force(raw_force)));
        let kind = PointerKind::Touch(finger_id);
        let source = PointerSource::Touch { finger_id, force };
        let button = ButtonSource::Touch { finger_id, force };

        match action {
            TouchEvent::Down => {
                let first_contact = self.input.pressed_fingers.is_empty();
                let newly_pressed = self.input.pressed_fingers.insert(raw_finger_id);
                if first_contact {
                    self.input.primary_pointer = Some(finger_id);
                }
                let primary = self.input.primary_pointer == Some(finger_id);
                if newly_pressed {
                    app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::PointerEntered {
                            device_id,
                            primary,
                            position,
                            kind,
                        },
                    );
                }
                app.window_event(
                    &self.window_target,
                    self.current_window,
                    event::WindowEvent::PointerButton {
                        device_id,
                        primary,
                        state: ElementState::Pressed,
                        position,
                        button,
                        is_macos_activation_click: false,
                    },
                );
            }
            TouchEvent::Move => app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerMoved {
                    device_id,
                    primary: self.input.primary_pointer == Some(finger_id),
                    position,
                    source,
                },
            ),
            TouchEvent::Up | TouchEvent::Cancel => {
                let primary = self.input.primary_pointer == Some(finger_id);
                self.input.pressed_fingers.remove(&raw_finger_id);
                if matches!(action, TouchEvent::Up) {
                    app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::PointerButton {
                            device_id,
                            primary,
                            state: ElementState::Released,
                            position,
                            button,
                            is_macos_activation_click: false,
                        },
                    );
                }
                app.window_event(
                    &self.window_target,
                    self.current_window,
                    event::WindowEvent::PointerLeft {
                        device_id,
                        primary,
                        position: Some(position),
                        kind,
                    },
                );
                if primary {
                    self.input.primary_pointer = None;
                }
            }
            TouchEvent::Unknown => {}
        }
    }

    fn update_window_drag(&self, position: PhysicalPosition<f64>, released: bool) {
        let mut state = self.window_target.shared.lock();
        state.pointer_position = position;
        if released {
            state.dragging = None;
            self.window_target.shared.geometry.cancel();
            return;
        }
        let Some(drag) = state.dragging else {
            return;
        };
        let screen = PhysicalPosition::new(
            position.x + f64::from(state.position.x),
            position.y + f64::from(state.position.y),
        );
        let dx = (screen.x - drag.pointer.x) as i32;
        let dy = (screen.y - drag.pointer.y) as i32;
        let mut origin = drag.position;
        let mut size = drag.size;
        let resize = drag.direction;
        if let Some(direction) = resize {
            use ResizeDirection::*;
            let west = matches!(direction, West | NorthWest | SouthWest);
            let east = matches!(direction, East | NorthEast | SouthEast);
            let north = matches!(direction, North | NorthEast | NorthWest);
            let south = matches!(direction, South | SouthEast | SouthWest);
            let min = state.min_size.unwrap_or(PhysicalSize::new(1, 1));
            let max = state
                .max_size
                .unwrap_or(PhysicalSize::new(u32::MAX, u32::MAX));
            if west || east {
                size.width = (i64::from(drag.size.width)
                    + if west { -i64::from(dx) } else { i64::from(dx) })
                .clamp(
                    i64::from(min.width.max(1)),
                    i64::from(max.width.max(min.width).max(1)),
                ) as u32;
            }
            if north || south {
                size.height = (i64::from(drag.size.height)
                    + if north { -i64::from(dy) } else { i64::from(dy) })
                .clamp(
                    i64::from(min.height.max(1)),
                    i64::from(max.height.max(min.height).max(1)),
                ) as u32;
            }
            if let Some(increments) = state.resize_increments {
                if increments.width > 0 {
                    size.width -= size.width % increments.width;
                }
                if increments.height > 0 {
                    size.height -= size.height % increments.height;
                }
            }
            if west {
                origin.x += drag.size.width as i32 - size.width as i32;
            }
            if north {
                origin.y += drag.size.height as i32 - size.height as i32;
            }
        } else {
            origin.x += dx;
            origin.y += dy;
        }
        drop(state);
        let window = Window {
            app: self.openharmony_app.clone(),
            shared: self.window_target.shared.clone(),
            id: self.current_window,
            root_claimed: self.window_target.root_claimed.clone(),
            owns_window: false,
        };
        window.submit_geometry(GeometryTarget {
            position: origin,
            size: resize.map(|_| size),
        });
    }

    fn handle_mouse_event<A: ApplicationHandler>(
        &mut self,
        mouse_event: &MouseEventData,
        app: &mut A,
    ) {
        let device_id = Some(DeviceId::from_raw(0));
        let position = PhysicalPosition::new(mouse_event.x as f64, mouse_event.y as f64);
        self.update_window_drag(
            position,
            matches!(
                mouse_event.action,
                OhosMouseAction::Release | OhosMouseAction::Cancel
            ),
        );

        match mouse_event.action {
            OhosMouseAction::Press | OhosMouseAction::Release | OhosMouseAction::Move => {
                self.ensure_mouse_entered(device_id, position, app);
            }
            _ => {}
        }

        match mouse_event.action {
            OhosMouseAction::Move => app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::PointerMoved {
                    device_id,
                    position,
                    primary: true,
                    source: PointerSource::Mouse,
                },
            ),
            OhosMouseAction::Press | OhosMouseAction::Release => {
                let button = map_mouse_button(mouse_event.button);
                let pressed = matches!(mouse_event.action, OhosMouseAction::Press);
                let buttons = &mut self.input.pressed_mouse_buttons;
                if pressed {
                    if !buttons.contains(&button) {
                        buttons.push(button.clone());
                    }
                } else {
                    buttons.retain(|pressed| pressed != &button);
                }
                app.window_event(
                    &self.window_target,
                    self.current_window,
                    event::WindowEvent::PointerButton {
                        device_id,
                        state: if pressed {
                            ElementState::Pressed
                        } else {
                            ElementState::Released
                        },
                        position,
                        primary: true,
                        button,
                        is_macos_activation_click: false,
                    },
                );
            }
            // The system took over the mouse stream (for example a native drag);
            // no Release follows, so end every press the app still considers held.
            OhosMouseAction::Cancel => {
                for button in std::mem::take(&mut self.input.pressed_mouse_buttons) {
                    app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::PointerButton {
                            device_id,
                            state: ElementState::Released,
                            position,
                            primary: true,
                            button,
                            is_macos_activation_click: false,
                        },
                    );
                }
            }
            OhosMouseAction::None => {
                trace!("Ignoring an OHOS mouse event with no actionable state")
            }
        }
    }

    fn ensure_mouse_entered<A: ApplicationHandler>(
        &mut self,
        device_id: Option<DeviceId>,
        position: PhysicalPosition<f64>,
        app: &mut A,
    ) {
        if self.input.mouse_inside {
            return;
        }
        self.input.mouse_inside = true;
        self.window_target.shared.lock().invalidate_cursor();
        app.window_event(
            &self.window_target,
            self.current_window,
            event::WindowEvent::PointerEntered {
                device_id,
                position,
                primary: true,
                kind: PointerKind::Mouse,
            },
        );
    }

    fn handle_ime_deletion<A: ApplicationHandler>(
        &mut self,
        backward: bool,
        count: usize,
        app: &mut A,
    ) {
        if count == 0 {
            return;
        }
        let (before, after) = if backward { (count, 0) } else { (0, count) };
        let (bytes, native) = {
            let mut state = self.window_target.shared.lock();
            let bytes = state
                .ime_surrounding
                .as_mut()
                .and_then(|text| text.delete(before, after));
            if state.native_ime_edits {
                state
                    .ime_edits
                    .push(crate::ImeEdit::DeleteSurrounding { before, after });
            }
            (bytes, state.native_ime_edits)
        };
        let ime = match bytes {
            Some((before_bytes, after_bytes)) => Ime::DeleteSurrounding {
                before_bytes,
                after_bytes,
            },
            // Native edit consumers drain `take_ime_edits` on every IME event.
            None if native => Ime::Enabled,
            None => {
                // Without surrounding text the deletion extent is unknown, and editors
                // such as terminals have no backing text at all. Key presses let the
                // application apply its own deletion rules.
                let key = if backward {
                    Keycode::Del
                } else {
                    Keycode::ForwardDel
                };
                for _ in 0..count {
                    self.emit_synthetic_key(key, app);
                }
                return;
            }
        };
        app.window_event(
            &self.window_target,
            self.current_window,
            event::WindowEvent::Ime(ime),
        );
    }

    fn emit_synthetic_key<A: ApplicationHandler>(&mut self, keycode: Keycode, app: &mut A) {
        for state in [ElementState::Pressed, ElementState::Released] {
            self.emit_key_event(
                keycode,
                state,
                Some(DeviceId::from_raw(0)),
                false,
                false,
                app,
            );
        }
    }

    fn emit_key_event<A: ApplicationHandler>(
        &mut self,
        keycode: Keycode,
        state: ElementState,
        device_id: Option<DeviceId>,
        repeat: bool,
        is_synthetic: bool,
        app: &mut A,
    ) {
        self.emit_key_event_with_unicode(
            keycode,
            state,
            device_id,
            repeat,
            is_synthetic,
            None,
            app,
        );
    }

    fn sync_key_modifiers<A: ApplicationHandler>(&mut self, key: &KeyboardEventData, app: &mut A) {
        let Some(pressed) = &key.pressed_keys else {
            return;
        };
        let before = Modifiers::new(self.input.modifiers, self.input.modifier_keys);
        self.input.modifiers = ModifiersState::empty();
        self.input.modifier_keys = ModifiersKeys::empty();
        for code in pressed {
            self.update_modifiers(*code, ElementState::Pressed);
        }
        let after = Modifiers::new(self.input.modifiers, self.input.modifier_keys);
        if before != after {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::ModifiersChanged(after),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_key_event_with_unicode<A: ApplicationHandler>(
        &mut self,
        keycode: Keycode,
        state: ElementState,
        device_id: Option<DeviceId>,
        repeat: bool,
        is_synthetic: bool,
        unicode: Option<char>,
        app: &mut A,
    ) {
        if let Some(modifiers) = self.update_modifiers(keycode, state) {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::ModifiersChanged(modifiers),
            );
        }

        let key_without_modifiers = keycodes::to_logical(keycode);
        let logical_key = unicode.map_or_else(
            || key_without_modifiers.clone(),
            |ch| winit_core::keyboard::Key::Character(ch.to_string().into()),
        );
        let text = if state.is_pressed() {
            logical_key.to_text().map(Into::into)
        } else {
            None
        };
        app.window_event(
            &self.window_target,
            self.current_window,
            event::WindowEvent::KeyboardInput {
                device_id,
                event: event::KeyEvent {
                    state,
                    physical_key: keycodes::to_physical_key(keycode),
                    logical_key: logical_key.clone(),
                    location: keycodes::to_location(keycode),
                    repeat,
                    text: text.clone(),
                    text_with_all_modifiers: text,
                    key_without_modifiers,
                },
                is_synthetic,
            },
        );
    }

    fn update_modifiers(&mut self, keycode: Keycode, state: ElementState) -> Option<Modifiers> {
        let (state_flag, key_flag) = match keycode {
            Keycode::ShiftLeft => (ModifiersState::SHIFT, ModifiersKeys::LSHIFT),
            Keycode::ShiftRight => (ModifiersState::SHIFT, ModifiersKeys::RSHIFT),
            Keycode::AltLeft => (ModifiersState::ALT, ModifiersKeys::LALT),
            Keycode::AltRight => (ModifiersState::ALT, ModifiersKeys::RALT),
            Keycode::CtrlLeft => (ModifiersState::CONTROL, ModifiersKeys::LCONTROL),
            Keycode::CtrlRight => (ModifiersState::CONTROL, ModifiersKeys::RCONTROL),
            Keycode::MetaLeft => (ModifiersState::META, ModifiersKeys::LMETA),
            Keycode::MetaRight => (ModifiersState::META, ModifiersKeys::RMETA),
            _ => return None,
        };
        let before = Modifiers::new(self.input.modifiers, self.input.modifier_keys);
        if state.is_pressed() {
            self.input.modifiers.insert(state_flag);
            self.input.modifier_keys.insert(key_flag);
        } else {
            self.input.modifiers.remove(state_flag);
            self.input.modifier_keys.remove(key_flag);
        }
        let after = Modifiers::new(self.input.modifiers, self.input.modifier_keys);
        (before != after).then_some(after)
    }

    pub fn run_app<A: ApplicationHandler + 'static>(
        mut self,
        app: A,
    ) -> Result<(), EventLoopError> {
        let openharmony_app = self.openharmony_app.clone();
        let mut app = Some(app);
        openharmony_app.run_loop(move |event| {
            let frame_only = self.window_target.main_wake.begin_turn();
            let _turn = crate::frame::enter_turn(match &event {
                MainEvent::WindowRedraw(_) | MainEvent::SubWindowRedraw { .. } => {
                    crate::frame::Turn::Frame
                }
                _ => crate::frame::Turn::Event,
            });
            let destroyed = matches!(event, MainEvent::Destroy);
            if let Some(app) = app.as_mut() {
                #[cfg(feature = "native-vsync")]
                let frames: Vec<_> = if matches!(event, MainEvent::UserEvent) {
                    self.registered_windows()
                        .into_iter()
                        .filter(|(_, shared)| shared.frame.native_callback_pending())
                        .map(|(id, _)| id)
                        .collect()
                } else {
                    Vec::new()
                };
                #[cfg(feature = "native-vsync")]
                let frame_only = frame_only && !frames.is_empty();
                // A pure NativeVSync wake becomes per-window frame iterations,
                // including the idle fast path. Coalesced event work runs first.
                if !frame_only || !matches!(event, MainEvent::UserEvent) {
                    self.single_iteration(LoopEvent::Ability(event), app);
                }
                #[cfg(feature = "native-vsync")]
                for id in frames {
                    let _turn = crate::frame::enter_turn(crate::frame::Turn::Frame);
                    self.single_iteration(LoopEvent::NativeFrame(id), app);
                }
            }
            // Ability retains this callback. Release application and GPU resources at
            // shutdown instead of keeping them alive in the callback forever.
            if (destroyed || self.window_target.exiting()) && app.take().is_some() {
                let bridge = self.openharmony_app.bridge();
                crate::task::spawn(async move {
                    use crate::plugins::app_control::{AppControlBridgePlugin, TerminateRequest, TerminateResponse};
                    let result = async {
                        bridge?.call_sync_from_worker::<AppControlBridgePlugin, TerminateRequest, TerminateResponse>(
                            "terminate", TerminateRequest { code: 0 },
                        ).await
                    }.await;
                    if let Err(error) = result { warn!("OHOS terminate: {error}"); }
                });
            }
        });
        Ok(())
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        _app: A,
    ) -> Result<(), EventLoopError> {
        Err(NotSupportedError::new(
            "run_app_on_demand is incompatible with the callback-driven OHOS Ability lifecycle",
        )
        .into())
    }

    pub fn pump_app_events<A: ApplicationHandler>(
        &mut self,
        _timeout: Option<Duration>,
        _app: A,
    ) -> PumpStatus {
        warn!("pump_app_events is not supported by the callback-driven OHOS Ability lifecycle");
        if self.window_target.exiting() {
            PumpStatus::Exit(0)
        } else {
            PumpStatus::Continue
        }
    }

    pub fn create_proxy(&self) -> CoreEventLoopProxy {
        self.window_target.create_proxy()
    }

    /// Applies delivery changes outside frame callbacks and returns the earliest
    /// fallback draw for windows whose callback registration failed.
    fn sync_frame_delivery(&self, flush: bool, now: Instant) -> Option<Instant> {
        let windows = self.window_target.windows.lock().unwrap();
        let mut deadline: Option<Instant> = None;
        for shared in windows.values() {
            if flush {
                shared.frame.flush(&self.openharmony_app, now);
            }
            if let Some(fallback) = shared.frame.fallback_deadline() {
                deadline = Some(deadline.map_or(fallback, |deadline| deadline.min(fallback)));
            }
        }
        deadline
    }

    fn dispatch_redraw<A: ApplicationHandler>(
        &self,
        id: WindowId,
        shared: &SharedWindowState,
        tick: bool,
        app: &mut A,
    ) {
        let redraw = {
            let mut state = shared.lock();
            let WindowState {
                lifecycle,
                visible,
                minimized,
                redraw_pending,
                ..
            } = &mut *state;
            lifecycle.take_redraw(tick, *visible, *minimized, redraw_pending)
        };
        if !redraw {
            return;
        }
        app.window_event(&self.window_target, id, event::WindowEvent::RedrawRequested);
        let mut state = shared.lock();
        if state.presentable() && state.resize_settle.active(Instant::now()) {
            state.redraw_pending = true;
            shared.frame.request();
        }
    }

    fn registered_windows(&self) -> Vec<(WindowId, Arc<SharedWindowState>)> {
        self.window_target
            .windows
            .lock()
            .unwrap()
            .iter()
            .map(|(id, shared)| (*id, shared.clone()))
            .collect()
    }

    fn request_window_redraw(&self, id: WindowId) {
        let shared = self.window_target.windows.lock().unwrap().get(&id).cloned();
        if let Some(shared) = shared {
            shared.request_redraw();
        }
    }

    fn clear_window_input(&mut self, id: WindowId) {
        if self.current_window == id {
            self.input = InputState::default();
        }
        self.input_states.remove(&id);
    }

    fn close_window<A: ApplicationHandler>(&mut self, id: WindowId, app: &mut A) {
        let shared = self.window_target.windows.lock().unwrap().get(&id).cloned();
        let Some(shared) = shared else {
            return;
        };
        let events = {
            let mut state = shared.lock();
            let events = state.lifecycle.close();
            state.dragging = None;
            shared.sync_active(&state);
            events
        };
        if !events.destroyed {
            return;
        }
        shared.geometry.cancel();
        crate::drag_drop::cancel_outgoing(Some(id));
        self.clear_window_input(id);
        if events.close_requested {
            app.window_event(&self.window_target, id, event::WindowEvent::CloseRequested);
        }
        app.window_event(&self.window_target, id, event::WindowEvent::Destroyed);
        self.window_target.windows.lock().unwrap().remove(&id);
    }

    fn single_iteration<A: ApplicationHandler>(&mut self, event: LoopEvent<'_>, app: &mut A) {
        if self.window_target.exiting() {
            self.timer.set_deadline(None);
            return;
        }
        let (event, native_frame): (Option<_>, Option<WindowId>) = match event {
            LoopEvent::Ability(event) => (Some(event), None),
            #[cfg(feature = "native-vsync")]
            LoopEvent::NativeFrame(id) => (None, Some(id)),
        };
        let id = native_frame.unwrap_or_else(|| {
            let id = match &event {
                Some(
                    MainEvent::WindowResize { window_id, .. }
                    | MainEvent::WindowFocusChanged { window_id, .. }
                    | MainEvent::SubWindowInput { window_id, .. }
                    | MainEvent::SubWindowRedraw { window_id, .. },
                ) => *window_id,
                Some(
                    MainEvent::SubWindowSurfaceCreate(id)
                    | MainEvent::SubWindowSurfaceDestroy(id)
                    | MainEvent::SubWindowClosed(id),
                ) => *id,
                Some(MainEvent::ContentRectChange(content)) => content.window_id,
                Some(MainEvent::AvoidAreaChange(info)) => info.window_id,
                _ => 0,
            };
            WindowId::from_raw(id as usize)
        });
        let frame_window = native_frame.or(match &event {
            Some(MainEvent::WindowRedraw(_) | MainEvent::SubWindowRedraw { .. }) => Some(id),
            _ => None,
        });
        let shared = self.window_target.windows.lock().unwrap().get(&id).cloned();
        if frame_window.is_some() {
            let Some(shared) = &shared else {
                return;
            };
            let delivered = match native_frame {
                #[cfg(feature = "native-vsync")]
                Some(_) => shared.frame.take_native_callback(),
                _ => shared.frame.on_callback(),
            };
            if !delivered {
                return;
            }
            let due = {
                let state = shared.lock();
                state.redraw_pending && state.presentable()
            } || (id == self.current_window && self.input.pending_touch.is_some());
            // Idle callbacks only count toward unregistration; they must not run a
            // full application iteration on every vsync.
            if !due && !matches!(self.window_target.control_flow(), ControlFlow::Poll) {
                return;
            }
        }

        let user_turn = matches!(&event, Some(MainEvent::UserEvent));
        let cause = if self.loop_running {
            start_cause(self.window_target.control_flow(), self.wait_started)
        } else {
            self.loop_running = true;
            StartCause::Init
        };
        let proxy_wake_up = user_turn && self.window_target.event_loop_proxy.take_wake_up();
        app.new_events(&self.window_target, cause);

        // A raw touch waits for the ArkUI metadata event that follows it in the same
        // input dispatch. Any other turn means that metadata is not coming.
        let input_turn = matches!(
            &event,
            Some(MainEvent::Input(_) | MainEvent::SubWindowInput { .. })
        );
        if !(input_turn && id == self.current_window)
            && let Some(touch) = self.input.pending_touch.take()
        {
            self.handle_touch_event(&touch, app);
        }
        if let Some(shared) = shared {
            if self.current_window != id {
                let old_input = std::mem::take(&mut self.input);
                if self
                    .window_target
                    .windows
                    .lock()
                    .unwrap()
                    .contains_key(&self.current_window)
                {
                    self.input_states.insert(self.current_window, old_input);
                }
                self.input = self.input_states.remove(&id).unwrap_or_default();
            }
            self.current_window = id;
            self.window_target.shared = shared;
        }

        for (id, status) in openharmony_ability::drain_pending_window_status() {
            let Some(status) = crate::window_status::window_status(status) else {
                warn!("Ignoring invalid OHOS window status: {status}");
                continue;
            };
            let id = WindowId::from_raw(id as usize);
            let shared = self.window_target.windows.lock().unwrap().get(&id).cloned();
            if let Some(shared) = shared {
                let (changed, occluded, restored) = {
                    let mut state = shared.lock();
                    let old_occluded = state.occluded();
                    let restored = state.minimized && !status.minimized;
                    state.minimized = status.minimized;
                    state.maximized = status.maximized;
                    state.fullscreen = status.fullscreen.then_some(Fullscreen::Borderless(None));
                    shared.sync_active(&state);
                    (old_occluded != state.occluded(), state.occluded(), restored)
                };
                if changed {
                    app.window_event(
                        &self.window_target,
                        id,
                        event::WindowEvent::Occluded(occluded),
                    );
                }
                if restored {
                    shared.request_redraw();
                }
            }
        }
        for id in openharmony_ability::drain_pending_window_closes() {
            let id = WindowId::from_raw(id as usize);
            if id == GLOBAL_WINDOW {
                if self.window_target.windows.lock().unwrap().contains_key(&id) {
                    app.window_event(&self.window_target, id, event::WindowEvent::CloseRequested);
                }
            } else {
                self.close_window(id, app);
            }
        }
        let registered = self.window_target.windows.lock().unwrap().contains_key(&id);
        if let Some(event) = event
            && (registered
                || matches!(
                    &event,
                    MainEvent::ConfigChanged(_)
                        | MainEvent::Start
                        | MainEvent::Stop
                        | MainEvent::Resume(_)
                        | MainEvent::Destroy
                        | MainEvent::LowMemory
                        | MainEvent::UserEvent
                        | MainEvent::NewWant { .. }
                        | MainEvent::Pause
                        | MainEvent::Create
                        | MainEvent::WindowCreate
                        | MainEvent::SaveState(_)
                ))
        {
            self.handle_main_event(event, app);
        }
        if proxy_wake_up {
            app.proxy_wake_up(&self.window_target);
        }
        let transfers = self.window_target.drag_drop.lock().unwrap().drain();
        for (id, event) in transfers {
            if self.window_target.windows.lock().unwrap().contains_key(&id) {
                app.window_event(&self.window_target, id, event);
            }
        }
        if let Some((id, event)) = crate::drag_drop::drain_outgoing()
            && self.window_target.windows.lock().unwrap().contains_key(&id)
        {
            app.window_event(&self.window_target, id, event);
        }
        if crate::events::take_pending() {
            self.request_window_redraw(GLOBAL_WINDOW);
        }
        if !self.window_target.exiting() {
            let now = Instant::now();
            if let Some(frame_window) = frame_window {
                let shared = self
                    .window_target
                    .windows
                    .lock()
                    .unwrap()
                    .get(&frame_window)
                    .cloned();
                if let Some(shared) = shared {
                    self.dispatch_redraw(frame_window, &shared, true, app);
                }
            } else if self
                .fallback_deadline
                .is_some_and(|deadline| deadline <= now)
            {
                for (id, shared) in self.registered_windows() {
                    let tick = shared.frame.begin_fallback(now);
                    self.dispatch_redraw(id, &shared, tick, app);
                }
            }
        }
        app.about_to_wait(&self.window_target);
        if self.window_target.exiting() {
            self.timer.set_deadline(None);
            self.surface_destroyed(app, true);
            return;
        }
        self.wait_started = Instant::now();
        use crate::control_flow::{Action, ControlFlow as Policy, control_flow_action};
        let flow = match self.window_target.control_flow() {
            ControlFlow::Wait => Policy::Wait,
            ControlFlow::WaitUntil(deadline) => Policy::WaitUntil(deadline),
            ControlFlow::Poll => Policy::Poll,
        };
        let presentable = matches!(flow, Policy::Poll)
            .then(|| {
                self.registered_windows()
                    .into_iter()
                    .find(|(_, shared)| shared.lock().presentable())
            })
            .flatten();
        let deadline = match control_flow_action(flow, self.wait_started, presentable.is_some()) {
            Action::Wait => None,
            Action::Deadline(deadline) => Some(deadline),
            Action::Wake => {
                self.window_target.main_wake.notify();
                None
            }
            Action::Frame => {
                if let Some((_, shared)) = presentable {
                    let state = shared.lock();
                    if state.presentable() {
                        shared.frame.request();
                    }
                }
                None
            }
        };
        self.fallback_deadline =
            self.sync_frame_delivery(frame_window.is_none(), self.wait_started);
        let deadline = match (deadline, self.fallback_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.timer.set_deadline(deadline);
    }

    fn handle_main_event<A: ApplicationHandler>(&mut self, event: MainEvent<'_>, app: &mut A) {
        match event {
            MainEvent::SurfaceCreate => self.surface_created(app),
            MainEvent::SurfaceDestroy => self.surface_destroyed(app, false),
            MainEvent::WindowResize { size, .. } => {
                self.update_surface_size(physical_size(size.width, size.height), app)
            }
            MainEvent::WindowRedraw(_) => {}
            MainEvent::ContentRectChange(content) => {
                let position = PhysicalPosition::new(content.rect.left, content.rect.top);
                let outer_size = physical_size(content.rect.width, content.rect.height);
                let (moved, resized) = {
                    let mut state = self.window_target.shared.lock();
                    let moved = state.position != position;
                    let resized = state.outer_size != outer_size;
                    state.position = position;
                    state.outer_size = outer_size;
                    if resized {
                        state.resize_settle.extend(Instant::now());
                    }
                    (moved, resized)
                };
                if moved {
                    app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::Moved(position),
                    );
                }
                if resized {
                    self.window_target.shared.request_redraw();
                }
            }
            MainEvent::AvoidAreaChange(_) => self.request_window_redraw(self.current_window),
            MainEvent::KeyboardEvent(_) => self.request_window_redraw(GLOBAL_WINDOW),
            MainEvent::WindowFocusChanged { focused, .. } => self.update_focus(focused, app),
            MainEvent::GainedFocus => self.update_focus(true, app),
            MainEvent::LostFocus => self.update_focus(false, app),
            MainEvent::ConfigChanged(configuration) => {
                self.update_configuration(&configuration, app)
            }
            MainEvent::LowMemory => app.memory_warning(&self.window_target),
            MainEvent::Start => self.update_visibility(true, app),
            MainEvent::Resume(_) => {
                self.update_visibility(true, app);
                self.ensure_resumed(app);
            }
            MainEvent::Pause => trace!("OHOS Ability paused"),
            MainEvent::Stop => {
                self.update_visibility(false, app);
                self.suspend(app);
            }
            MainEvent::WindowDestroy => app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::CloseRequested,
            ),
            MainEvent::Destroy => {
                self.timer.set_deadline(None);
                self.surface_destroyed(app, true);
                let mut windows = self.registered_windows();
                windows.sort_by_key(|(id, _)| (*id == GLOBAL_WINDOW, id.into_raw()));
                for (id, shared) in windows {
                    shared.lock().lifecycle.closed = true;
                    app.window_event(&self.window_target, id, event::WindowEvent::Destroyed);
                    self.window_target.windows.lock().unwrap().remove(&id);
                }
                self.window_target.exit();
            }
            MainEvent::Input(input_event) => self.handle_input_event(&input_event, app),
            MainEvent::Create | MainEvent::WindowCreate | MainEvent::SaveState(_) => {}
            MainEvent::UserEvent => {}
            MainEvent::SubWindowSurfaceCreate(_) => {
                {
                    let mut state = self.window_target.shared.lock();
                    state.lifecycle.surface_created();
                    state.resize_settle.extend(Instant::now());
                    state.invalidate_cursor();
                    self.window_target.shared.sync_active(&state);
                }
                self.synchronize_window_state();
                if let Some(ready) = self.window_target.shared.ready.lock().unwrap().take() {
                    let _ = ready.send(());
                }
                let attrs = self.window_target.shared.lock().initial_attributes.take();
                if let Some(attrs) = attrs {
                    let window = Window {
                        app: self.openharmony_app.clone(),
                        shared: self.window_target.shared.clone(),
                        id: self.current_window,
                        root_claimed: self.window_target.root_claimed.clone(),
                        owns_window: false,
                    };
                    window.configure(attrs);
                }
                app.can_create_surfaces(&self.window_target);
                self.window_target.shared.request_redraw();
            }
            MainEvent::SubWindowInput { event, .. } => self.handle_input_event(&event, app),
            MainEvent::SubWindowRedraw { .. } => {}
            MainEvent::SubWindowSurfaceDestroy(_) => {
                let destroyed = {
                    let mut state = self.window_target.shared.lock();
                    let events = state.lifecycle.surface_destroyed();
                    state.dragging = None;
                    self.window_target.shared.sync_active(&state);
                    events.destroyed
                };
                self.window_target.shared.geometry.cancel();
                crate::drag_drop::cancel_outgoing(Some(self.current_window));
                self.clear_window_input(self.current_window);
                if destroyed {
                    app.window_event(
                        &self.window_target,
                        self.current_window,
                        event::WindowEvent::Destroyed,
                    );
                    self.window_target
                        .windows
                        .lock()
                        .unwrap()
                        .remove(&self.current_window);
                }
            }
            MainEvent::SubWindowClosed(_) => self.close_window(self.current_window, app),
            MainEvent::NewWant { uri } => {
                crate::events::emit(crate::events::PlatformEvent::Want {
                    uri,
                    parameters: self.openharmony_app.take_want_parameters(),
                });
                self.request_window_redraw(GLOBAL_WINDOW);
            }
        }
    }

    fn surface_created<A: ApplicationHandler>(&mut self, app: &mut A) {
        let uri = self.openharmony_app.take_initial_want_uri();
        let parameters = self.openharmony_app.take_want_parameters();
        {
            let mut state = self.window_target.shared.lock();
            state.lifecycle.surface_created();
            state.resize_settle.extend(Instant::now());
            state.invalidate_cursor();
            self.window_target.shared.sync_active(&state);
        }
        self.synchronize_window_state();
        self.ensure_resumed(app);
        if !self.surface_available {
            self.surface_available = true;
            app.can_create_surfaces(&self.window_target);
        }
        self.window_target.shared.request_redraw();
        if !uri.is_empty() || !parameters.is_empty() {
            crate::events::emit(crate::events::PlatformEvent::Want { uri, parameters });
        }
    }

    fn synchronize_window_state(&self) {
        let content_rect = self
            .openharmony_app
            .content_rect_for(self.current_window.into_raw() as i64);
        let window_rect = self
            .openharmony_app
            .window_rect_for(self.current_window.into_raw() as i64);
        let surface_size = physical_size(content_rect.width, content_rect.height);
        let outer_size = physical_size(window_rect.width, window_rect.height);
        let mut state = self.window_target.shared.lock();
        state.surface_size = surface_size;
        state.surface_position = PhysicalPosition::new(content_rect.left, content_rect.top);
        state.outer_size = nonzero_size_or(outer_size, surface_size);
        state.position = PhysicalPosition::new(window_rect.left, window_rect.top);
        state.scale_factor = scale_factor(&self.openharmony_app);
        state.theme = theme(self.openharmony_app.config().color_mode);
    }

    fn surface_destroyed<A: ApplicationHandler>(&mut self, app: &mut A, all: bool) {
        crate::drag_drop::cancel_outgoing(if all { None } else { Some(GLOBAL_WINDOW) });
        if self.surface_available
            || (all
                && self
                    .registered_windows()
                    .iter()
                    .any(|(_, shared)| shared.lock().lifecycle.surface_available))
        {
            app.destroy_surfaces(&self.window_target);
            self.surface_available = false;
        }
        for (id, shared) in self.registered_windows() {
            if !all && id != GLOBAL_WINDOW {
                continue;
            }
            let mut state = shared.lock();
            state.lifecycle.surface_available = false;
            state.surface_size = PhysicalSize::new(0, 0);
            state.dragging = None;
            shared.sync_active(&state);
            shared.geometry.cancel();
            drop(state);
            self.clear_window_input(id);
        }
        self.suspend(app);
    }

    fn ensure_resumed<A: ApplicationHandler>(&mut self, app: &mut A) {
        if !self.resumed {
            self.resumed = true;
            app.resumed(&self.window_target);
        }
    }

    fn suspend<A: ApplicationHandler>(&mut self, app: &mut A) {
        if self.resumed {
            self.resumed = false;
            app.suspended(&self.window_target);
        }
    }

    fn update_focus<A: ApplicationHandler>(&mut self, focused: bool, app: &mut A) {
        let mut changes = Vec::new();
        for (id, shared) in self.registered_windows() {
            if !focused && id != self.current_window {
                continue;
            }
            let next = focused && id == self.current_window;
            let changed = {
                let mut state = shared.lock();
                let changed = state.focused != next;
                state.focused = next;
                changed
            };
            changes.push((id, next, changed));
        }
        changes.sort_by_key(|(_, focused, _)| *focused);
        for (id, focused, changed) in changes {
            if changed {
                app.window_event(
                    &self.window_target,
                    id,
                    event::WindowEvent::Focused(focused),
                );
            }
            let input = if id == self.current_window {
                Some(&mut self.input)
            } else {
                self.input_states.get_mut(&id)
            };
            if !focused
                && let Some(input) = input
                && (!input.modifiers.is_empty() || !input.modifier_keys.is_empty())
            {
                input.modifiers = ModifiersState::empty();
                input.modifier_keys = ModifiersKeys::empty();
                app.window_event(
                    &self.window_target,
                    id,
                    event::WindowEvent::ModifiersChanged(Modifiers::default()),
                );
            }
        }
    }

    fn update_visibility<A: ApplicationHandler>(&self, visible: bool, app: &mut A) {
        self.window_target.visible.set(visible);
        for (id, shared) in self.registered_windows() {
            let (changed, occluded) = {
                let mut state = shared.lock();
                let old = state.occluded();
                state.visible = visible;
                shared.sync_active(&state);
                (old != state.occluded(), state.occluded())
            };
            if changed {
                app.window_event(
                    &self.window_target,
                    id,
                    event::WindowEvent::Occluded(occluded),
                );
            }
            if visible {
                shared.request_redraw();
            }
        }
    }

    fn update_surface_size<A: ApplicationHandler>(&self, size: PhysicalSize<u32>, app: &mut A) {
        let content_rect = self
            .openharmony_app
            .content_rect_for(self.current_window.into_raw() as i64);
        // Window-manager resize notifications include system bars. Only XComponent
        // geometry describes the EGL surface; its callback updates this rect first.
        let size = nonzero_size_or(physical_size(content_rect.width, content_rect.height), size);
        let changed = {
            let mut state = self.window_target.shared.lock();
            let changed = state.surface_size != size;
            state.surface_size = size;
            state.surface_position = PhysicalPosition::new(content_rect.left, content_rect.top);
            if changed {
                state.resize_settle.extend(Instant::now());
            }
            if state.outer_size == PhysicalSize::new(0, 0) {
                state.outer_size = size;
            }
            changed
        };
        if changed {
            app.window_event(
                &self.window_target,
                self.current_window,
                event::WindowEvent::SurfaceResized(size),
            );
        }
        // Repeated notifications cost one frame, not another settle window.
        self.window_target.shared.request_redraw();
    }

    fn update_configuration<A: ApplicationHandler>(
        &self,
        configuration: &Configuration,
        app: &mut A,
    ) {
        let scale_factor = scale_factor(&self.openharmony_app);
        let theme = theme(configuration.color_mode);
        for (id, shared) in self.registered_windows() {
            let (old_scale, old_theme, current_size) = {
                let mut state = shared.lock();
                let old = (state.scale_factor, state.theme, state.surface_size);
                state.scale_factor = scale_factor;
                state.theme = theme;
                old
            };
            if (scale_factor - old_scale).abs() > f64::EPSILON {
                let requested_size = Arc::new(Mutex::new(current_size));
                app.window_event(
                    &self.window_target,
                    id,
                    event::WindowEvent::ScaleFactorChanged {
                        surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(
                            &requested_size,
                        )),
                        scale_factor,
                    },
                );
                if *requested_size.lock().unwrap() != current_size {
                    trace!("ignoring a surface-size request during an OHOS scale-factor change");
                }
            }
            if theme != old_theme
                && let Some(theme) = theme
            {
                app.window_event(
                    &self.window_target,
                    id,
                    event::WindowEvent::ThemeChanged(theme),
                );
            }
            shared.request_redraw();
        }
    }
}

impl EventLoopProvider for EventLoop {
    fn run_app<A: ApplicationHandler + 'static>(self, app: A) -> Result<(), EventLoopError> {
        EventLoop::run_app(self, app)
    }

    fn create_proxy(&self) -> CoreEventLoopProxy {
        EventLoop::create_proxy(self)
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        self.window_target.owned_display_handle()
    }

    fn listen_device_events(&self, allowed: DeviceEvents) {
        self.window_target.listen_device_events(allowed);
    }

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.window_target.set_control_flow(control_flow);
    }

    fn create_custom_cursor(
        &self,
        source: CustomCursorSource,
    ) -> Result<CustomCursor, CustomCursorError> {
        self.window_target.create_custom_cursor(source)
    }
}

impl EventLoopExtRegister for EventLoop {
    fn register_app<A: ApplicationHandler + 'static>(self, app: A) {
        let _ = self.run_app(app);
    }
}

pub struct EventLoopProxy {
    wake_up: AtomicBool,
    main_wake: Arc<crate::main_wake::MainWake>,
}

impl fmt::Debug for EventLoopProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventLoopProxy")
            .field("wake_up", &self.wake_up)
            .finish_non_exhaustive()
    }
}

impl EventLoopProxy {
    pub fn new(app: OpenHarmonyApp) -> Self {
        Self {
            wake_up: AtomicBool::new(false),
            main_wake: crate::frame::main_wake(&app),
        }
    }

    fn take_wake_up(&self) -> bool {
        self.wake_up.swap(false, Ordering::AcqRel)
    }
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        self.wake_up.store(true, Ordering::Release);
        self.main_wake.notify();
    }
}

#[derive(Debug)]
pub struct ActiveEventLoop {
    pub(crate) app: OpenHarmonyApp,
    main_wake: Arc<crate::main_wake::MainWake>,
    visible: Cell<bool>,
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    event_loop_proxy: Arc<EventLoopProxy>,
    shared: Arc<SharedWindowState>,
    windows: Mutex<HashMap<WindowId, Arc<SharedWindowState>>>,
    drag_drop: Mutex<crate::drag_drop::DragDrop>,
    root_claimed: Arc<AtomicBool>,
}

impl RootActiveEventLoop for ActiveEventLoop {
    fn start_drag(
        &self,
        source: WindowId,
        send_data: Box<dyn winit_core::data_transfer::DataTransferSend>,
        actions: &[winit_core::event_loop::DndAction],
        icon: Option<winit_core::event_loop::DragIcon>,
    ) -> Result<winit_core::data_transfer::DataTransferId, winit_core::error::TransferError> {
        use winit_core::data_transfer::{SendData, TypeHint};
        use winit_core::error::TransferError;
        if !actions.iter().any(|action| {
            matches!(
                action,
                winit_core::event_loop::DndAction::Copy | winit_core::event_loop::DndAction::Move
            )
        }) {
            return Err(TransferError::NotSupported(NotSupportedError::new(
                "OHOS file drags support Copy or Move",
            )));
        }
        let uris = match send_data.data_for_type(&TypeHint::UriList) {
            Some(SendData::Uris(uris)) => uris,
            Some(SendData::String(text)) => crate::drag_drop::parse_uris(&text),
            Some(SendData::Bytes(bytes)) => crate::drag_drop::parse_uris(
                &String::from_utf8(bytes).map_err(|_| TransferError::Failed)?,
            ),
            _ => return Err(TransferError::Failed),
        };
        let preview = icon
            .map(|icon| {
                let image = icon
                    .icon
                    .cast_ref::<winit_core::icon::RgbaIcon>()
                    .ok_or(TransferError::Failed)?;
                let preview = openharmony_ability::DragPreview {
                    rgba: image.buffer().to_vec(),
                    width: image.width(),
                    height: image.height(),
                    hotspot_x: -(icon.offset_x as f32),
                    hotspot_y: -(icon.offset_y as f32),
                };
                preview.validate().map_err(|_| TransferError::Failed)?;
                Ok::<_, TransferError>(preview)
            })
            .transpose()?;
        crate::drag_drop::start_file_drag_with_options(
            &self.app,
            source,
            uris,
            actions.to_vec(),
            preview,
        )
    }

    fn create_proxy(&self) -> CoreEventLoopProxy {
        CoreEventLoopProxy::new(self.event_loop_proxy.clone())
    }

    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, CreateWindowError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn data_transfer(
        &self,
        id: winit_core::data_transfer::DataTransferId,
    ) -> Result<Box<dyn winit_core::data_transfer::DataTransfer>, winit_core::error::TransferError>
    {
        self.drag_drop.lock().unwrap().data(id)
    }
    fn fetch_data_transfer(
        &self,
        id: winit_core::data_transfer::DataTransferId,
        ty: &dyn winit_core::data_transfer::TransferType,
    ) -> Result<winit_core::event_loop::AsyncRequestSerial, winit_core::error::TransferError> {
        let result = self.drag_drop.lock().unwrap().fetch(id, ty);
        self.main_wake.notify();
        result
    }
    fn set_valid_dnd_actions(
        &self,
        id: winit_core::data_transfer::DataTransferId,
        actions: &[winit_core::event_loop::DndAction],
    ) -> Result<(), winit_core::error::TransferError> {
        self.drag_drop.lock().unwrap().accept(id, actions)
    }

    fn create_custom_cursor(
        &self,
        source: CustomCursorSource,
    ) -> Result<CustomCursor, CustomCursorError> {
        match source {
            CustomCursorSource::Image(image) if image.width() <= 256 && image.height() <= 256 => {
                Ok(CustomCursor(Arc::new(crate::cursor::ImageCursor(image))))
            }
            _ => Err(CustomCursorError::NotSupported),
        }
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(crate::monitor::all().into_iter())
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        crate::monitor::primary()
    }

    fn system_theme(&self) -> Option<Theme> {
        self.shared.lock().theme
    }

    fn listen_device_events(&self, _allowed: DeviceEvents) {}

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.control_flow.set(control_flow)
    }

    fn control_flow(&self) -> ControlFlow {
        self.control_flow.get()
    }

    fn exit(&self) {
        self.exit.set(true)
    }

    fn exiting(&self) -> bool {
        self.exit.get()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        CoreOwnedDisplayHandle::new(Arc::new(OwnedDisplayHandle))
    }

    fn rwh_06_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }
}

impl rwh_06::HasDisplayHandle for ActiveEventLoop {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::OhosDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedDisplayHandle;

impl rwh_06::HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::OhosDisplayHandle::new();
        Ok(unsafe { rwh_06::DisplayHandle::borrow_raw(raw.into()) })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformSpecificWindowAttributes;

impl window::PlatformWindowAttributes for PlatformSpecificWindowAttributes {
    fn box_clone(&self) -> Box<dyn window::PlatformWindowAttributes> {
        Box::new(*self)
    }
}

#[derive(Debug)]
pub struct Window {
    app: OpenHarmonyApp,
    shared: Arc<SharedWindowState>,
    id: WindowId,
    root_claimed: Arc<AtomicBool>,
    owns_window: bool,
}

impl Window {
    pub(crate) fn new(
        el: &ActiveEventLoop,
        attrs: window::WindowAttributes,
    ) -> Result<Self, CreateWindowError> {
        let root = !el.root_claimed.swap(true, Ordering::AcqRel);
        let id = if root {
            GLOBAL_WINDOW
        } else {
            let size = attrs
                .surface_size
                .unwrap_or(Size::Physical(PhysicalSize::new(800, 600)))
                .to_physical::<u32>(scale_factor(&el.app));
            let position = attrs
                .position
                .unwrap_or(Position::Physical(PhysicalPosition::new(100, 100)))
                .to_physical::<i32>(scale_factor(&el.app));
            let id = openharmony_ability::window::create_os_window(
                openharmony_ability::window::WindowCreateParams {
                    name: attrs.title.clone(),
                    native_module_name: el.app.module_name(),
                    width: size.width as i32,
                    height: size.height as i32,
                    x: position.x,
                    y: position.y,
                    decorations: attrs.decorations,
                    transparent: attrs.transparent,
                    ..Default::default()
                },
            )
            .map_err(|err| {
                CreateWindowError::Os(OsError::new(
                    line!(),
                    file!(),
                    std::io::Error::other(err.to_string()),
                ))
            })?;
            WindowId::from_raw(id as usize)
        };
        let shared = if root {
            el.windows
                .lock()
                .unwrap()
                .get(&GLOBAL_WINDOW)
                .unwrap()
                .clone()
        } else {
            let shared = Arc::new(SharedWindowState::new(
                &el.app,
                id.into_raw() as i64,
                el.visible.get(),
            ));
            shared.lock().scale_factor = scale_factor(&el.app);
            el.windows.lock().unwrap().insert(id, shared.clone());
            shared
        };
        let window = Self {
            app: el.app.clone(),
            shared,
            id,
            root_claimed: el.root_claimed.clone(),
            owns_window: true,
        };
        if root {
            window.configure(attrs);
        } else {
            window.shared.lock().initial_attributes = Some(attrs);
        }
        Ok(window)
    }

    fn configure(&self, attrs: WindowAttributes) {
        let window = self;
        window.set_title(&attrs.title);
        if attrs.window_icon.is_some() {
            window.set_window_icon(attrs.window_icon.clone());
        }
        window.set_resizable(attrs.resizable);
        window.set_decorations(attrs.decorations);
        window.set_enabled_buttons(attrs.enabled_buttons);
        window.set_min_surface_size(attrs.min_surface_size);
        window.set_max_surface_size(attrs.max_surface_size);
        if attrs.transparent {
            window.set_transparent(true);
        }
        if attrs.blur {
            window.set_blur(true);
        }
        if attrs.maximized {
            window.set_maximized(true);
        }
        if let Some(fullscreen) = attrs.fullscreen {
            window.set_fullscreen(Some(fullscreen));
        }
        if let Some(size) = attrs.surface_size {
            let _ = window.request_surface_size(size);
        }
        if let Some(position) = attrs.position {
            window.set_outer_position(position);
        }
        window.command("resolve window id", |client, state, id| async move {
            let id = client.get_real_window_id(id).await?;
            state.lock().real_window_id = i32::try_from(id).ok();
            Ok(())
        });
    }

    fn command<F, Fut>(&self, name: &'static str, operation: F)
    where
        F: FnOnce(crate::plugins::window::WindowClient, Arc<SharedWindowState>, i64) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = openharmony_ability::napi_ohos::Result<()>>
            + Send
            + 'static,
    {
        let client = crate::plugins::window::WindowClient::new(&self.app);
        let state = self.shared.clone();
        let id = self.id.into_raw() as i64;
        self.shared.commands.push(async move {
            let result = match client {
                Ok(client) => operation(client, state, id).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                warn!("OHOS {name}: {error}");
            }
        });
    }

    fn submit_geometry(&self, target: GeometryTarget) {
        {
            let state = self.shared.lock();
            if state.dragging.is_none()
                || !state.presentable()
                || !self.shared.geometry.submit(target)
            {
                return;
            }
        }
        let shared = self.shared.clone();
        let client = crate::plugins::window::WindowClient::new(&self.app);
        let id = self.id.into_raw() as i64;
        self.shared.commands.push(async move {
            while let Some(target) = shared.geometry.take() {
                let result = async {
                    let client = client.as_ref().map_err(|error| {
                        openharmony_ability::napi_ohos::Error::from_reason(error.to_string())
                    })?;
                    client
                        .move_window_to(id, target.position.x.into(), target.position.y.into())
                        .await?;
                    if let Some(size) = target.size {
                        let border = {
                            let state = shared.lock();
                            PhysicalSize::new(
                                state
                                    .outer_size
                                    .width
                                    .saturating_sub(state.surface_size.width),
                                state
                                    .outer_size
                                    .height
                                    .saturating_sub(state.surface_size.height),
                            )
                        };
                        client
                            .resize_window(
                                id,
                                i64::from(size.width) + i64::from(border.width),
                                i64::from(size.height) + i64::from(border.height),
                            )
                            .await?;
                    }
                    Ok::<_, openharmony_ability::napi_ohos::Error>(())
                }
                .await;
                if let Err(error) = result {
                    warn!("OHOS drag geometry: {error}");
                }
            }
        });
    }

    fn apply_limits(&self) {
        let state = self.shared.lock();
        let min = state.min_size.unwrap_or_default();
        let max = state.max_size.unwrap_or_default();
        drop(state);
        self.command("window size limits", move |client, _, id| async move {
            client
                .set_window_limits(
                    id,
                    min.width.into(),
                    min.height.into(),
                    max.width.into(),
                    max.height.into(),
                )
                .await
        });
    }

    fn apply_buttons(&self) {
        let state = self.shared.lock();
        let buttons = state.enabled_buttons;
        let flags = i32::from(buttons.contains(WindowButtons::CLOSE))
            | (i32::from(buttons.contains(WindowButtons::MAXIMIZE)) << 1)
            | (i32::from(buttons.contains(WindowButtons::MINIMIZE)) << 2)
            | (i32::from(state.resizable) << 3);
        drop(state);
        self.command("window buttons", move |client, _, id| async move {
            client.set_window_decoration_flags(id, flags).await
        });
    }

    fn update_ime(&self, data: window::ImeRequestData) {
        use openharmony_ability::ime::{EnterKey, InputType, Rect};
        if let Some(text) = data.surrounding_text {
            let (cursor_byte, anchor_byte) = (text.cursor(), text.anchor());
            let text = text.into_text();
            let cursor = text[..cursor_byte].encode_utf16().count();
            let anchor = text[..anchor_byte].encode_utf16().count();
            self.shared.lock().ime_surrounding = Some(crate::ime_text::SurroundingText::new(
                text.clone(),
                cursor_byte,
                anchor_byte,
            ));
            let _ = self.app.update_ime_text_state_for(
                self.id.into_raw() as i64,
                openharmony_ability::ime::TextState {
                    text,
                    offset: 0,
                    selection: cursor.min(anchor)..cursor.max(anchor),
                    reversed: cursor < anchor,
                },
            );
        }

        if let Some((hint, purpose)) = data.hint_and_purpose {
            let input_type = match purpose {
                ImePurpose::Password => InputType::VisiblePassword,
                _ if hint.contains(window::ImeHint::MULTILINE) => InputType::Multiline,
                _ => InputType::Text,
            };
            if let Err(error) = self.app.configure_ime_for(
                self.id.into_raw() as i64,
                if hint.contains(window::ImeHint::MULTILINE) {
                    EnterKey::Newline
                } else {
                    EnterKey::Done
                },
                input_type,
            ) {
                warn!("Cannot configure OHOS IME: {error}");
            }
        }
        if let Some((position, size)) = data.cursor_area {
            let state = self.shared.lock();
            let position = position.to_physical::<f64>(state.scale_factor);
            let size = size.to_physical::<f64>(state.scale_factor);
            let rect = Rect {
                left: f64::from(state.position.x + state.surface_position.x) + position.x,
                top: f64::from(state.position.y + state.surface_position.y) + position.y,
                width: size.width,
                height: size.height,
            };
            drop(state);
            if let Err(error) = self
                .app
                .update_ime_cursor_for(self.id.into_raw() as i64, rect)
            {
                warn!("Cannot position OHOS IME: {error}");
            }
        }
    }

    pub fn request_redraw(&self) {
        self.shared.request_redraw();
    }

    pub fn scale_factor(&self) -> f64 {
        self.shared.lock().scale_factor
    }

    pub fn take_ime_edits(&self) -> Vec<crate::ImeEdit> {
        let mut state = self.shared.lock();
        state.native_ime_edits = true;
        std::mem::take(&mut state.ime_edits)
    }

    pub fn config(&self) -> Configuration {
        self.app.config()
    }

    pub fn content_rect(&self) -> Rect {
        self.app.content_rect_for(self.id.into_raw() as i64)
    }

    pub fn openharmony_app(&self) -> &OpenHarmonyApp {
        &self.app
    }

    // Allow the usage of HasRawWindowHandle inside this function
    #[allow(deprecated)]
    pub fn raw_window_handle_rwh_06(&self) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        if !self.shared.lock().lifecycle.surface_available {
            return Err(rwh_06::HandleError::Unavailable);
        }
        if let Some(handle) = self
            .app
            .native_window_for(self.id.into_raw() as i64)
            .and_then(|window| window.raw_window_handle())
        {
            return Ok(handle);
        }

        tracing::trace!(
            "The OHOS native window is only available between can_create_surfaces and \
             destroy_surfaces callbacks"
        );
        Err(rwh_06::HandleError::Unavailable)
    }

    fn raw_display_handle_rwh_06(&self) -> Result<rwh_06::RawDisplayHandle, rwh_06::HandleError> {
        Ok(rwh_06::RawDisplayHandle::Ohos(
            rwh_06::OhosDisplayHandle::new(),
        ))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if !self.owns_window {
            return;
        }
        if self.id == GLOBAL_WINDOW {
            self.root_claimed.store(false, Ordering::Release);
        } else {
            {
                let mut state = self.shared.lock();
                if state.lifecycle.closed {
                    return;
                }
                state.lifecycle.dropped_by_app = true;
                state.dragging = None;
                self.shared.sync_active(&state);
                self.shared.geometry.cancel();
            }
            let wake = crate::frame::main_wake(&self.app);
            self.command("destroy window", move |client, _, id| async move {
                client.destroy_window(id).await?;
                openharmony_ability::notify_window_close(id as i32);
                wake.notify();
                Ok(())
            });
        }
    }
}

impl rwh_06::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_display_handle_rwh_06()?;
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let raw = self.raw_window_handle_rwh_06()?;
        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw)) }
    }
}

impl CoreWindow for Window {
    fn window_type(&self) -> WindowType {
        WindowType::Window
    }

    fn id(&self) -> WindowId {
        self.id
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        crate::monitor::primary()
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(crate::monitor::all().into_iter())
    }

    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        let id = self
            .shared
            .lock()
            .real_window_id
            .and_then(|id| {
                ohos_window_manager_binding::Window::from_id(id)
                    .properties()
                    .ok()
            })
            .map(|p| u64::from(p.display_id));
        crate::monitor::all()
            .into_iter()
            .find(|monitor| Some(monitor.native_id()) == id)
            .or_else(crate::monitor::primary)
    }

    fn scale_factor(&self) -> f64 {
        self.shared.lock().scale_factor
    }

    fn request_redraw(&self) {
        Window::request_redraw(self);
    }

    fn pre_present_notify(&self) {}

    fn surface_position(&self) -> PhysicalPosition<i32> {
        self.shared.lock().surface_position
    }

    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        Ok(self.shared.lock().position)
    }

    fn set_outer_position(&self, position: Position) {
        let position = position.to_physical::<i32>(self.scale_factor());
        self.command("move window", move |client, _, id| async move {
            client
                .move_window_to(id, position.x.into(), position.y.into())
                .await
        });
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        self.shared.lock().surface_size
    }

    fn request_surface_size(&self, size: Size) -> Option<PhysicalSize<u32>> {
        let size = size.to_physical::<u32>(self.scale_factor());
        let state = self.shared.lock();
        let border = PhysicalSize::new(
            state
                .outer_size
                .width
                .saturating_sub(state.surface_size.width),
            state
                .outer_size
                .height
                .saturating_sub(state.surface_size.height),
        );
        drop(state);
        self.command("resize window", move |client, _, id| async move {
            client
                .resize_window(
                    id,
                    i64::from(size.width) + i64::from(border.width),
                    i64::from(size.height) + i64::from(border.height),
                )
                .await
        });
        None
    }

    fn outer_size(&self) -> PhysicalSize<u32> {
        self.shared.lock().outer_size
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        if self.id == GLOBAL_WINDOW {
            safe_area(&self.app)
        } else {
            PhysicalInsets::default()
        }
    }

    fn set_min_surface_size(&self, size: Option<Size>) {
        let size = size.map(|size| size.to_physical(self.scale_factor()));
        self.shared.lock().min_size = size;
        self.apply_limits();
    }

    fn set_max_surface_size(&self, size: Option<Size>) {
        let size = size.map(|size| size.to_physical(self.scale_factor()));
        self.shared.lock().max_size = size;
        self.apply_limits();
    }

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        self.shared.lock().resize_increments
    }

    fn set_surface_resize_increments(&self, increments: Option<Size>) {
        let increments = increments.map(|size| size.to_physical(self.scale_factor()));
        self.shared.lock().resize_increments = increments;
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        self.command("window title", move |client, state, id| async move {
            client.set_window_title(id, title.clone()).await?;
            state.lock().title = title;
            Ok(())
        });
    }

    fn set_transparent(&self, transparent: bool) {
        self.command("window background", move |client, _, id| async move {
            client
                .set_window_background_color(id, if transparent { 0 } else { 0xff000000 })
                .await
        });
    }

    fn set_blur(&self, blur: bool) {
        self.command("window blur", move |client, _, id| async move {
            client
                .set_window_blur(id, if blur { 30.0 } else { 0.0 })
                .await
        });
    }

    fn set_visible(&self, visible: bool) {
        self.command("window visibility", move |client, _, id| async move {
            if visible {
                client.show_window(id).await
            } else {
                client.minimize_window(id).await
            }
        });
    }

    fn is_visible(&self) -> Option<bool> {
        Some(self.shared.lock().visible)
    }

    fn set_resizable(&self, resizable: bool) {
        self.shared.lock().resizable = resizable;
        self.apply_buttons();
    }

    fn is_resizable(&self) -> bool {
        self.shared.lock().resizable
    }

    fn set_enabled_buttons(&self, buttons: WindowButtons) {
        self.shared.lock().enabled_buttons = buttons;
        self.apply_buttons();
    }

    fn enabled_buttons(&self) -> WindowButtons {
        self.shared.lock().enabled_buttons
    }

    fn set_minimized(&self, minimized: bool) {
        self.command("minimize window", move |client, _, id| async move {
            if minimized {
                client.minimize_window(id).await?;
            } else {
                client.show_window(id).await?;
            }
            Ok(())
        });
    }

    fn is_minimized(&self) -> Option<bool> {
        Some(self.shared.lock().minimized)
    }

    fn set_maximized(&self, maximized: bool) {
        self.command("maximize window", move |client, state, id| async move {
            if maximized {
                client.maximize_window(id).await?;
            } else {
                client.recover_window(id).await?;
            }
            state.lock().maximized = maximized;
            Ok(())
        });
    }

    fn is_maximized(&self) -> bool {
        self.shared.lock().maximized
    }

    fn set_fullscreen(&self, fullscreen: Option<Fullscreen>) {
        self.command("fullscreen", move |client, state, id| async move {
            client.set_fullscreen(id, fullscreen.is_some()).await?;
            state.lock().fullscreen = fullscreen;
            Ok(())
        });
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        self.shared.lock().fullscreen.clone()
    }

    fn set_decorations(&self, decorations: bool) {
        self.command("decorations", move |client, state, id| async move {
            client.set_window_decorations(id, decorations).await?;
            state.lock().decorations = decorations;
            Ok(())
        });
    }

    fn is_decorated(&self) -> bool {
        self.shared.lock().decorations
    }

    fn set_window_level(&self, level: WindowLevel) {
        self.command("window level", move |client, _, id| async move {
            client
                .set_window_topmost(id, level == WindowLevel::AlwaysOnTop)
                .await
        });
    }

    fn set_window_icon(&self, window_icon: Option<winit_core::icon::Icon>) {
        if self.id != GLOBAL_WINDOW {
            warn!("OHOS child windows share their Ability icon");
            return;
        }
        let (size, rgba) = if let Some(icon) = window_icon {
            let Some(icon) = icon.0.cast_ref::<winit_core::icon::RgbaIcon>() else {
                return;
            };
            let size = icon.width().max(icon.height());
            if size > 1024 {
                warn!("OHOS window icon exceeds 1024 pixels");
                return;
            }
            let mut rgba = vec![0; (size * size * 4) as usize];
            let offset_x = (size - icon.width()) / 2;
            let offset_y = (size - icon.height()) / 2;
            for y in 0..icon.height() {
                let start = ((y + offset_y) * size + offset_x) as usize * 4;
                let source = (y * icon.width()) as usize * 4;
                rgba[start..start + icon.width() as usize * 4]
                    .copy_from_slice(&icon.buffer()[source..source + icon.width() as usize * 4]);
            }
            (size, rgba)
        } else {
            (0, Vec::new())
        };
        self.command("window icon", move |client, _, id| async move {
            client
                .set_window_icon(crate::plugins::window::WindowIconRequest {
                    window_id: id,
                    size,
                    rgba,
                })
                .await
        });
    }

    fn request_ime_update(&self, request: ImeRequest) -> Result<(), ImeRequestError> {
        match request {
            ImeRequest::Enable(enable) => {
                let (capabilities, data) = enable.into_raw();
                let mut state = self.shared.lock();
                if state.ime_capabilities.is_some() {
                    return Err(ImeRequestError::AlreadyEnabled);
                }
                state.ime_capabilities = Some(capabilities);
                state.ime_surrounding = None;
                drop(state);
                self.update_ime(data);
                if let Err(error) = self.app.try_show_keyboard_for(self.id.into_raw() as i64) {
                    self.shared.lock().ime_capabilities = None;
                    warn!("Cannot show OHOS keyboard: {error}");
                    return Err(ImeRequestError::NotSupported);
                }
            }
            ImeRequest::Update(data) => {
                if self.shared.lock().ime_capabilities.is_none() {
                    return Err(ImeRequestError::NotEnabled);
                }
                self.update_ime(data);
            }
            ImeRequest::Disable => {
                let mut state = self.shared.lock();
                state.ime_capabilities = None;
                state.ime_surrounding = None;
                drop(state);
                self.app.hide_keyboard_for(self.id.into_raw() as i64);
            }
            _ => return Err(ImeRequestError::NotSupported),
        }

        Ok(())
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        self.shared.lock().ime_capabilities
    }

    fn focus_window(&self) {
        self.command("focus window", |client, _, id| async move {
            client.focus_window(id).await
        });
    }

    fn request_user_attention(&self, request_type: Option<window::UserAttentionType>) {
        if request_type.is_some() {
            self.command("request attention", |client, _, id| async move {
                client.request_user_attention(id).await
            });
        }
    }

    fn set_cursor(&self, cursor: Cursor) {
        let key = match &cursor {
            Cursor::Icon(icon) => CursorKey::Icon(crate::cursor::style(*icon)),
            Cursor::Custom(custom)
                if custom.0.cast_ref::<crate::cursor::ImageCursor>().is_some() =>
            {
                CursorKey::Custom(custom.clone())
            }
            _ => return,
        };
        let generation = {
            let mut state = self.shared.lock();
            if state.cursor_pending.as_ref() == Some(&key)
                || (state.cursor_pending.is_none() && state.cursor_key.as_ref() == Some(&key))
            {
                return;
            }
            state.cursor_pending = Some(key.clone());
            state.cursor_generation
        };
        let client = crate::plugins::window::WindowClient::new(&self.app);
        let shared = self.shared.clone();
        let id = self.id.into_raw() as i64;
        self.shared.commands.push(async move {
            let result = async {
                let client = client?;
                match cursor {
                    Cursor::Icon(icon) => {
                        client.set_cursor_icon(id, crate::cursor::style(icon)).await
                    }
                    Cursor::Custom(custom) => {
                        let image = &custom.0.cast_ref::<crate::cursor::ImageCursor>().unwrap().0;
                        client
                            .set_custom_cursor(crate::plugins::window::CustomCursorRequest {
                                window_id: id,
                                rgba: image.buffer().to_vec(),
                                width: u32::from(image.width()),
                                height: u32::from(image.height()),
                                hotspot_x: u32::from(image.hotspot_x()),
                                hotspot_y: u32::from(image.hotspot_y()),
                            })
                            .await
                    }
                }
            }
            .await;
            let mut state = shared.lock();
            match result {
                Ok(()) if state.cursor_generation == generation => {
                    state.cursor_key = Some(key.clone());
                    if state.cursor_pending.as_ref() == Some(&key) {
                        state.cursor_pending = None;
                    }
                }
                Err(error) => {
                    state.invalidate_cursor();
                    warn!("OHOS cursor: {error}");
                }
                _ => {}
            }
        });
    }

    fn set_cursor_position(&self, _: Position) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_position is not supported").into())
    }

    fn set_cursor_grab(&self, mode: CursorGrabMode) -> Result<(), RequestError> {
        let id = self.shared.lock().real_window_id.ok_or_else(|| {
            RequestError::NotSupported(NotSupportedError::new("OHOS window id is not ready"))
        })?;
        let window = ohos_window_manager_binding::Window::from_id(id);
        let result = match mode {
            CursorGrabMode::None => window.unlock_cursor(),
            CursorGrabMode::Confined => window.lock_cursor(true),
            CursorGrabMode::Locked => window.lock_cursor(false),
        };
        result.map_err(|error| {
            RequestError::Os(OsError::new(
                line!(),
                file!(),
                std::io::Error::other(error.to_string()),
            ))
        })
    }

    fn set_cursor_visible(&self, visible: bool) {
        self.command("cursor visibility", move |client, _, _id| async move {
            client.set_cursor_visible(visible).await
        });
    }

    fn drag_window(&self) -> Result<(), RequestError> {
        let mut state = self.shared.lock();
        state.dragging = Some(WindowDrag {
            direction: None,
            pointer: PhysicalPosition::new(
                state.pointer_position.x + f64::from(state.position.x),
                state.pointer_position.y + f64::from(state.position.y),
            ),
            position: state.position,
            size: state.surface_size,
        });
        Ok(())
    }

    fn drag_resize_window(&self, direction: ResizeDirection) -> Result<(), RequestError> {
        if !self.is_resizable() {
            return Err(NotSupportedError::new("window is not resizable").into());
        }
        self.drag_window()?;
        if let Some(drag) = &mut self.shared.lock().dragging {
            drag.direction = Some(direction);
        }
        Ok(())
    }

    fn show_window_menu(&self, position: Position) {
        use crate::plugins::window::{WindowClient, WindowPromptAnchor, WindowPromptRequest};
        let position = position.to_logical::<f64>(self.scale_factor());
        let app = self.app.clone();
        let id = self.id.into_raw() as i64;
        let maximized = self.is_maximized();
        let fullscreen = self.fullscreen().is_some();
        crate::task::spawn(async move {
            let result = async {
                let client = WindowClient::new(&app)?;
                let selected = client
                    .show_prompt(WindowPromptRequest {
                        window_id: id,
                        message: "Window".into(),
                        detail: None,
                        level: 0,
                        buttons: vec![
                            "Minimize".into(),
                            if maximized { "Restore" } else { "Maximize" }.into(),
                            if fullscreen {
                                "Leave fullscreen"
                            } else {
                                "Fullscreen"
                            }
                            .into(),
                            "Close".into(),
                            "Cancel".into(),
                        ],
                        cancel_index: Some(4),
                        anchor: Some(WindowPromptAnchor {
                            x: position.x.max(0.0),
                            y: position.y.max(0.0),
                        }),
                    })
                    .await?;
                match selected {
                    0 => client.minimize_window(id).await?,
                    1 if maximized => client.recover_window(id).await?,
                    1 => client.maximize_window(id).await?,
                    2 => client.set_fullscreen(id, !fullscreen).await?,
                    3 => {
                        if id != 0 {
                            client.destroy_window(id).await?;
                        }
                        openharmony_ability::notify_window_close(id as i32);
                        crate::frame::main_wake(&app).notify();
                    }
                    _ => {}
                }
                Ok::<_, openharmony_ability::napi_ohos::Error>(())
            }
            .await;
            if let Err(error) = result {
                warn!("OHOS window menu: {error}");
            }
        });
    }

    fn set_cursor_hittest(&self, hittest: bool) -> Result<(), RequestError> {
        self.command("cursor hit test", move |client, _, id| async move {
            client.set_window_touchable(id, hittest).await
        });
        Ok(())
    }

    fn set_theme(&self, theme: Option<Theme>) {
        use crate::plugins::app_control::{
            AppControlBridgePlugin, SetColorModeRequest, SetColorModeResponse,
        };
        let bridge = self.app.bridge();
        crate::task::spawn(async move {
            let result = async { bridge?.call_sync_from_worker::<AppControlBridgePlugin, SetColorModeRequest, SetColorModeResponse>("set-color-mode", SetColorModeRequest { color_mode: match theme { Some(Theme::Dark) => 0, Some(Theme::Light) => 1, None => 2 } }).await }.await;
            if let Err(error) = result {
                warn!("OHOS color mode: {error}");
            }
        });
    }

    fn theme(&self) -> Option<Theme> {
        self.shared.lock().theme
    }

    fn set_content_protected(&self, protected: bool) {
        self.command("content protection", move |client, _, id| async move {
            let id = client.get_real_window_id(id).await?;
            ohos_window_manager_binding::Window::from_id(id as i32)
                .set_privacy_mode(protected)
                .map_err(|error| {
                    openharmony_ability::napi_ohos::Error::from_reason(error.to_string())
                })
        });
    }

    fn has_focus(&self) -> bool {
        self.shared.lock().focused
    }

    fn title(&self) -> String {
        self.shared.lock().title.clone()
    }

    fn reset_dead_keys(&self) {}

    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }

    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }
}

fn physical_size(width: i32, height: i32) -> PhysicalSize<u32> {
    PhysicalSize::new(nonnegative_u32(width), nonnegative_u32(height))
}

fn scale_factor(app: &OpenHarmonyApp) -> f64 {
    sanitize_scale_factor(app.scale() as f64)
}

fn sanitize_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

fn normalized_force(force: f32) -> f64 {
    if force.is_finite() {
        force.clamp(0.0, 1.0) as f64
    } else {
        0.0
    }
}

fn touch_phase_from_arkui(action: UIInputAction) -> TouchPhase {
    match action {
        UIInputAction::Down => TouchPhase::Started,
        UIInputAction::Move => TouchPhase::Moved,
        UIInputAction::Up => TouchPhase::Ended,
        UIInputAction::Cancel => TouchPhase::Cancelled,
    }
}

fn touch_phase_from_gesture(phase: OhosGesturePhase) -> TouchPhase {
    match phase {
        OhosGesturePhase::Start => TouchPhase::Started,
        OhosGesturePhase::Update => TouchPhase::Moved,
        OhosGesturePhase::End => TouchPhase::Ended,
        OhosGesturePhase::Cancel => TouchPhase::Cancelled,
    }
}

fn nonnegative_u32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn nonzero_size_or(preferred: PhysicalSize<u32>, fallback: PhysicalSize<u32>) -> PhysicalSize<u32> {
    if preferred.width == 0 || preferred.height == 0 {
        fallback
    } else {
        preferred
    }
}

fn theme(color_mode: ColorMode) -> Option<Theme> {
    match color_mode {
        ColorMode::Dark => Some(Theme::Dark),
        ColorMode::Light => Some(Theme::Light),
        ColorMode::NoSet => None,
    }
}

fn map_mouse_button(button: OhosMouseButton) -> ButtonSource {
    let button = match button {
        OhosMouseButton::LeftButton => MouseButton::Left,
        OhosMouseButton::RightButton => MouseButton::Right,
        OhosMouseButton::MiddleButton => MouseButton::Middle,
        OhosMouseButton::BackButton => MouseButton::Back,
        OhosMouseButton::ForwardButton => MouseButton::Forward,
        OhosMouseButton::NoneButton => return ButtonSource::Unknown(0),
    };
    ButtonSource::Mouse(button)
}

fn safe_area(app: &OpenHarmonyApp) -> PhysicalInsets<u32> {
    let mut top = 0;
    let mut left = 0;
    let mut bottom = 0;
    let mut right = 0;
    let content = app.content_rect();
    let vertical = |rect: Rect| {
        if rect.width <= 0
            || i64::from(rect.left) >= i64::from(content.left) + i64::from(content.width)
            || i64::from(rect.left) + i64::from(rect.width) <= i64::from(content.left)
        {
            return (0, 0);
        }
        crate::safe_area::edge_insets(content.top, content.height, rect.top, rect.height)
    };
    let horizontal = |rect: Rect| {
        if rect.height <= 0
            || i64::from(rect.top) >= i64::from(content.top) + i64::from(content.height)
            || i64::from(rect.top) + i64::from(rect.height) <= i64::from(content.top)
        {
            return (0, 0);
        }
        crate::safe_area::edge_insets(content.left, content.width, rect.left, rect.width)
    };

    for (area_type, area) in app.avoid_areas() {
        if !area.visible || matches!(area_type, AvoidAreaType::Keyboard) {
            continue;
        }
        top = top.max(vertical(area.top_rect).0);
        left = left.max(horizontal(area.left_rect).0);
        bottom = bottom.max(vertical(area.bottom_rect).1);
        right = right.max(horizontal(area.right_rect).1);
    }

    PhysicalInsets::new(top, left, bottom, right)
}

fn start_cause(control_flow: ControlFlow, start: Instant) -> StartCause {
    let now = Instant::now();
    match control_flow {
        ControlFlow::Poll => StartCause::Poll,
        ControlFlow::Wait => StartCause::WaitCancelled {
            start,
            requested_resume: None,
        },
        ControlFlow::WaitUntil(deadline) if now >= deadline => StartCause::ResumeTimeReached {
            start,
            requested_resume: deadline,
        },
        ControlFlow::WaitUntil(deadline) => StartCause::WaitCancelled {
            start,
            requested_resume: Some(deadline),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_sizes_do_not_wrap_negative_values() {
        assert_eq!(physical_size(-1, 20), PhysicalSize::new(0, 20));
    }

    #[test]
    fn invalid_scale_factors_fall_back_to_one() {
        assert_eq!(sanitize_scale_factor(0.0), 1.0);
        assert_eq!(sanitize_scale_factor(f64::NAN), 1.0);
        assert_eq!(sanitize_scale_factor(2.5), 2.5);
    }

    #[test]
    fn touch_force_is_finite_and_normalized() {
        assert_eq!(normalized_force(f32::NAN), 0.0);
        assert_eq!(normalized_force(-1.0), 0.0);
        assert_eq!(normalized_force(2.0), 1.0);
    }

    #[test]
    fn arkui_input_phases_map_to_winit_phases() {
        assert_eq!(
            touch_phase_from_arkui(UIInputAction::Down),
            TouchPhase::Started
        );
        assert_eq!(
            touch_phase_from_arkui(UIInputAction::Move),
            TouchPhase::Moved
        );
        assert_eq!(touch_phase_from_arkui(UIInputAction::Up), TouchPhase::Ended);
        assert_eq!(
            touch_phase_from_arkui(UIInputAction::Cancel),
            TouchPhase::Cancelled
        );
    }

    #[test]
    fn arkui_gesture_phases_map_to_winit_phases() {
        assert_eq!(
            touch_phase_from_gesture(OhosGesturePhase::Start),
            TouchPhase::Started
        );
        assert_eq!(
            touch_phase_from_gesture(OhosGesturePhase::Update),
            TouchPhase::Moved
        );
        assert_eq!(
            touch_phase_from_gesture(OhosGesturePhase::End),
            TouchPhase::Ended
        );
        assert_eq!(
            touch_phase_from_gesture(OhosGesturePhase::Cancel),
            TouchPhase::Cancelled
        );
    }

    #[test]
    fn color_mode_maps_to_winit_theme() {
        assert_eq!(theme(ColorMode::Dark), Some(Theme::Dark));
        assert_eq!(theme(ColorMode::Light), Some(Theme::Light));
        assert_eq!(theme(ColorMode::NoSet), None);
    }
}
