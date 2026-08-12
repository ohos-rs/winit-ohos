use std::cell::Cell;
use std::collections::HashSet;
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
    AvoidAreaType, ColorMode, Configuration, Event as MainEvent, ImeEvent, InputEvent,
    OpenHarmonyApp, Rect, ime::KeyboardStatus,
};

use dpi::{PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{Cursor, CustomCursor, CustomCursorSource};
use winit_core::error::{EventLoopError, NotSupportedError, OsError, RequestError};
use winit_core::event::{
    self, ButtonSource, DeviceId, ElementState, FingerId, Force, Ime, Modifiers, MouseButton,
    PointerKind, PointerSource, StartCause, SurfaceSizeWriter,
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

#[derive(Debug)]
pub struct EventLoop {
    pub openharmony_app: OpenHarmonyApp,
    window_target: ActiveEventLoop,
    loop_running: bool,
    surface_available: bool,
    resumed: bool,
    primary_pointer: Option<FingerId>,
    pressed_fingers: HashSet<i32>,
    mouse_inside: bool,
    modifiers: ModifiersState,
    modifier_keys: ModifiersKeys,
    wait_started: Instant,
}

#[derive(Debug)]
struct SharedWindowState {
    inner: Mutex<WindowState>,
}

#[derive(Debug)]
struct WindowState {
    surface_size: PhysicalSize<u32>,
    surface_position: PhysicalPosition<i32>,
    outer_size: PhysicalSize<u32>,
    position: PhysicalPosition<i32>,
    scale_factor: f64,
    focused: bool,
    visible: bool,
    theme: Option<Theme>,
    ime_capabilities: Option<ImeCapabilities>,
}

impl SharedWindowState {
    fn new(app: &OpenHarmonyApp) -> Self {
        let content_rect = app.content_rect();
        let window_rect = app.window_rect();
        let surface_size = physical_size(content_rect.width, content_rect.height);
        let outer_size = physical_size(window_rect.width, window_rect.height);

        Self {
            inner: Mutex::new(WindowState {
                surface_size,
                surface_position: PhysicalPosition::new(content_rect.left, content_rect.top),
                outer_size: nonzero_size_or(outer_size, surface_size),
                position: PhysicalPosition::new(window_rect.left, window_rect.top),
                // The display API may not be ready while `#[ability]` is still initializing.
                scale_factor: 1.0,
                focused: false,
                visible: false,
                theme: theme(app.config().color_mode),
                ime_capabilities: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, WindowState> {
        self.inner.lock().unwrap()
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
        let shared = Arc::new(SharedWindowState::new(openharmony_app));

        Ok(Self {
            openharmony_app: openharmony_app.clone(),
            window_target: ActiveEventLoop {
                app: openharmony_app.clone(),
                control_flow: Cell::new(ControlFlow::default()),
                exit: Cell::new(false),
                event_loop_proxy,
                shared,
            },
            loop_running: false,
            surface_available: false,
            resumed: false,
            primary_pointer: None,
            pressed_fingers: HashSet::new(),
            mouse_inside: false,
            modifiers: ModifiersState::empty(),
            modifier_keys: ModifiersKeys::empty(),
            wait_started: Instant::now(),
        })
    }

    pub fn window_target(&self) -> &dyn RootActiveEventLoop {
        &self.window_target
    }

    fn handle_input_event<A: ApplicationHandler>(&mut self, event: &InputEvent, app: &mut A) {
        match event {
            InputEvent::TouchEvent(motion_event) => self.handle_touch_event(motion_event, app),
            InputEvent::MouseEvent(mouse_event) => self.handle_mouse_event(mouse_event, app),
            InputEvent::KeyEvent(key) => match key.action {
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
            InputEvent::ImeEvent(data) => match data {
                ImeEvent::TextInputEvent(s) => {
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
                        event::WindowEvent::Ime(Ime::Commit(s.text.clone())),
                    );
                }
                ImeEvent::BackspaceEvent(_) => {
                    self.emit_synthetic_key(Keycode::Del, app);
                }
                ImeEvent::EnterEvent(_) => self.emit_synthetic_key(Keycode::Enter, app),
                ImeEvent::ImeStatusEvent(status) => match status {
                    KeyboardStatus::Hide => {
                        app.window_event(
                            &self.window_target,
                            GLOBAL_WINDOW,
                            event::WindowEvent::Ime(Ime::Disabled),
                        );
                    }
                    KeyboardStatus::Show => {
                        app.window_event(
                            &self.window_target,
                            GLOBAL_WINDOW,
                            event::WindowEvent::Ime(Ime::Enabled),
                        );
                    }
                    KeyboardStatus::None => trace!("Ignoring an empty OHOS IME status event"),
                },
            },
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
        let finger_id = FingerId::from_raw(usize::try_from(raw_finger_id).unwrap_or(0));
        let device_id = Some(DeviceId::from_raw(raw_device_id));
        let position = PhysicalPosition::new(x as f64, y as f64);
        let force = Some(Force::Normalized(normalized_force(raw_force)));
        let kind = PointerKind::Touch(finger_id);
        let source = PointerSource::Touch { finger_id, force };
        let button = ButtonSource::Touch { finger_id, force };

        match action {
            TouchEvent::Down => {
                let first_contact = self.pressed_fingers.is_empty();
                let newly_pressed = self.pressed_fingers.insert(raw_finger_id);
                if first_contact {
                    self.primary_pointer = Some(finger_id);
                }
                let primary = self.primary_pointer == Some(finger_id);
                if newly_pressed {
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
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
                    GLOBAL_WINDOW,
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
                GLOBAL_WINDOW,
                event::WindowEvent::PointerMoved {
                    device_id,
                    primary: self.primary_pointer == Some(finger_id),
                    position,
                    source,
                },
            ),
            TouchEvent::Up | TouchEvent::Cancel => {
                let primary = self.primary_pointer == Some(finger_id);
                self.pressed_fingers.remove(&raw_finger_id);
                if matches!(action, TouchEvent::Up) {
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
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
                    GLOBAL_WINDOW,
                    event::WindowEvent::PointerLeft {
                        device_id,
                        primary,
                        position: Some(position),
                        kind,
                    },
                );
                if primary {
                    self.primary_pointer = None;
                }
            }
            TouchEvent::Unknown => {}
        }
    }

    fn handle_mouse_event<A: ApplicationHandler>(
        &mut self,
        mouse_event: &MouseEventData,
        app: &mut A,
    ) {
        let device_id = Some(DeviceId::from_raw(0));
        let position = PhysicalPosition::new(mouse_event.x as f64, mouse_event.y as f64);

        match mouse_event.action {
            OhosMouseAction::Press | OhosMouseAction::Release | OhosMouseAction::Move => {
                self.ensure_mouse_entered(device_id, position, app);
            }
            _ => {}
        }

        match mouse_event.action {
            OhosMouseAction::Move => app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::PointerMoved {
                    device_id,
                    position,
                    primary: true,
                    source: PointerSource::Mouse,
                },
            ),
            OhosMouseAction::Press | OhosMouseAction::Release => app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::PointerButton {
                    device_id,
                    state: if matches!(mouse_event.action, OhosMouseAction::Press) {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    },
                    position,
                    primary: true,
                    button: map_mouse_button(mouse_event.button),
                    is_macos_activation_click: false,
                },
            ),
            _ => trace!("Ignoring an OHOS mouse event with no actionable state"),
        }
    }

    fn ensure_mouse_entered<A: ApplicationHandler>(
        &mut self,
        device_id: Option<DeviceId>,
        position: PhysicalPosition<f64>,
        app: &mut A,
    ) {
        if self.mouse_inside {
            return;
        }
        self.mouse_inside = true;
        app.window_event(
            &self.window_target,
            GLOBAL_WINDOW,
            event::WindowEvent::PointerEntered {
                device_id,
                position,
                primary: true,
                kind: PointerKind::Mouse,
            },
        );
    }

    fn emit_synthetic_key<A: ApplicationHandler>(&mut self, keycode: Keycode, app: &mut A) {
        for state in [ElementState::Pressed, ElementState::Released] {
            self.emit_key_event(
                keycode,
                state,
                Some(DeviceId::from_raw(0)),
                false,
                true,
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
        if let Some(modifiers) = self.update_modifiers(keycode, state) {
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::ModifiersChanged(modifiers),
            );
        }

        let logical_key = keycodes::to_logical(keycode);
        let text = if state.is_pressed() {
            logical_key.to_text().map(Into::into)
        } else {
            None
        };
        app.window_event(
            &self.window_target,
            GLOBAL_WINDOW,
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
                    key_without_modifiers: logical_key,
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
        let before = Modifiers::new(self.modifiers, self.modifier_keys);
        if state.is_pressed() {
            self.modifiers.insert(state_flag);
            self.modifier_keys.insert(key_flag);
        } else {
            self.modifiers.remove(state_flag);
            self.modifier_keys.remove(key_flag);
        }
        let after = Modifiers::new(self.modifiers, self.modifier_keys);
        (before != after).then_some(after)
    }

    pub fn run_app<A: ApplicationHandler + 'static>(
        mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        let openharmony_app = self.openharmony_app.clone();
        openharmony_app.run_loop(move |event| self.single_iteration(event, &mut app));
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

    fn single_iteration<A: ApplicationHandler>(&mut self, event: MainEvent<'_>, app: &mut A) {
        if self.window_target.exiting() {
            return;
        }

        let cause = if self.loop_running {
            start_cause(self.window_target.control_flow(), self.wait_started)
        } else {
            self.loop_running = true;
            StartCause::Init
        };
        let proxy_wake_up = matches!(&event, MainEvent::UserEvent)
            && self.window_target.event_loop_proxy.take_wake_up();
        app.new_events(&self.window_target, cause);
        self.handle_main_event(event, app);
        if proxy_wake_up {
            app.proxy_wake_up(&self.window_target);
        }
        app.about_to_wait(&self.window_target);
        if self.window_target.exiting() {
            // Ability owns the process loop and exposes no unregister operation. Stop delivering
            // events, but first release resources whose lifetime is tied to the render surface.
            self.surface_destroyed(app);
            return;
        }
        self.wait_started = Instant::now();
    }

    fn handle_main_event<A: ApplicationHandler>(&mut self, event: MainEvent<'_>, app: &mut A) {
        match event {
            MainEvent::SurfaceCreate => self.surface_created(app),
            MainEvent::SurfaceDestroy => self.surface_destroyed(app),
            MainEvent::WindowResize(size) => {
                self.update_surface_size(physical_size(size.width, size.height), app)
            }
            MainEvent::WindowRedraw(_) => app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::RedrawRequested,
            ),
            MainEvent::ContentRectChange(content) => {
                let position = PhysicalPosition::new(content.rect.left, content.rect.top);
                let outer_size = physical_size(content.rect.width, content.rect.height);
                let moved = {
                    let mut state = self.window_target.shared.lock();
                    let moved = state.position != position;
                    state.position = position;
                    state.outer_size = outer_size;
                    moved
                };
                if moved {
                    app.window_event(
                        &self.window_target,
                        GLOBAL_WINDOW,
                        event::WindowEvent::Moved(position),
                    );
                }
            }
            MainEvent::AvoidAreaChange(_) | MainEvent::KeyboardEvent(_) => {}
            MainEvent::GainedFocus => self.update_focus(true, app),
            MainEvent::LostFocus => self.update_focus(false, app),
            MainEvent::ConfigChanged(configuration) => {
                self.update_configuration(&configuration, app)
            }
            MainEvent::LowMemory => app.memory_warning(&self.window_target),
            MainEvent::Start => self.update_visibility(true, app),
            MainEvent::Resume(_) => self.ensure_resumed(app),
            MainEvent::Pause => trace!("OHOS Ability paused"),
            MainEvent::Stop => {
                self.update_visibility(false, app);
                self.suspend(app);
            }
            MainEvent::WindowDestroy => app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::CloseRequested,
            ),
            MainEvent::Destroy => {
                self.surface_destroyed(app);
                app.window_event(
                    &self.window_target,
                    GLOBAL_WINDOW,
                    event::WindowEvent::Destroyed,
                );
            }
            MainEvent::Input(input_event) => self.handle_input_event(&input_event, app),
            MainEvent::Create | MainEvent::WindowCreate | MainEvent::SaveState(_) => {}
            MainEvent::UserEvent => {}
        }
    }

    fn surface_created<A: ApplicationHandler>(&mut self, app: &mut A) {
        self.synchronize_window_state();
        self.ensure_resumed(app);
        if !self.surface_available {
            self.surface_available = true;
            app.can_create_surfaces(&self.window_target);
        }
    }

    fn synchronize_window_state(&self) {
        let content_rect = self.openharmony_app.content_rect();
        let window_rect = self.openharmony_app.window_rect();
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

    fn surface_destroyed<A: ApplicationHandler>(&mut self, app: &mut A) {
        if self.surface_available {
            app.destroy_surfaces(&self.window_target);
            self.surface_available = false;
        }
        self.pressed_fingers.clear();
        self.primary_pointer = None;
        self.mouse_inside = false;
        self.window_target.shared.lock().surface_size = PhysicalSize::new(0, 0);
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
        let changed = {
            let mut state = self.window_target.shared.lock();
            let changed = state.focused != focused;
            state.focused = focused;
            changed
        };
        if changed {
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::Focused(focused),
            );
        }

        if !focused && (!self.modifiers.is_empty() || !self.modifier_keys.is_empty()) {
            self.modifiers = ModifiersState::empty();
            self.modifier_keys = ModifiersKeys::empty();
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::ModifiersChanged(Modifiers::default()),
            );
        }
    }

    fn update_visibility<A: ApplicationHandler>(&self, visible: bool, app: &mut A) {
        let changed = {
            let mut state = self.window_target.shared.lock();
            let changed = state.visible != visible;
            state.visible = visible;
            changed
        };
        if changed {
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::Occluded(!visible),
            );
        }
    }

    fn update_surface_size<A: ApplicationHandler>(&self, size: PhysicalSize<u32>, app: &mut A) {
        let content_rect = self.openharmony_app.content_rect();
        let changed = {
            let mut state = self.window_target.shared.lock();
            let changed = state.surface_size != size;
            state.surface_size = size;
            state.surface_position = PhysicalPosition::new(content_rect.left, content_rect.top);
            if state.outer_size == PhysicalSize::new(0, 0) {
                state.outer_size = size;
            }
            changed
        };
        if changed {
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::SurfaceResized(size),
            );
        }
    }

    fn update_configuration<A: ApplicationHandler>(
        &self,
        configuration: &Configuration,
        app: &mut A,
    ) {
        let scale_factor = scale_factor(&self.openharmony_app);
        let theme = theme(configuration.color_mode);
        let (old_scale_factor, old_theme, current_size) = {
            let state = self.window_target.shared.lock();
            (state.scale_factor, state.theme, state.surface_size)
        };

        if (scale_factor - old_scale_factor).abs() > f64::EPSILON {
            let requested_size = Arc::new(Mutex::new(current_size));
            self.window_target.shared.lock().scale_factor = scale_factor;
            app.window_event(
                &self.window_target,
                GLOBAL_WINDOW,
                event::WindowEvent::ScaleFactorChanged {
                    surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(&requested_size)),
                    scale_factor,
                },
            );
            let requested_size = *requested_size.lock().unwrap();
            if requested_size != current_size {
                trace!(
                    ?requested_size,
                    "ignoring a surface-size request during an OHOS scale-factor change"
                );
            }
        }

        if theme != old_theme {
            self.window_target.shared.lock().theme = theme;
            if let Some(theme) = theme {
                app.window_event(
                    &self.window_target,
                    GLOBAL_WINDOW,
                    event::WindowEvent::ThemeChanged(theme),
                );
            }
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
    ) -> Result<CustomCursor, RequestError> {
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
    app: OpenHarmonyApp,
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
            app,
        }
    }

    fn take_wake_up(&self) -> bool {
        self.wake_up.swap(false, Ordering::AcqRel)
    }
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        if !self.wake_up.swap(true, Ordering::AcqRel) {
            // Ability's waker queues a task; OHOS later executes it on the main thread and delivers
            // `UserEvent`. Obtain the current waker here because Ability installs it after the
            // `#[ability]` initializer returns. Coalesce repeated wake requests until that task is
            // consumed instead of filling the system queue with redundant main-thread work.
            self.app.create_waker().wake();
        }
    }
}

#[derive(Debug)]
pub struct ActiveEventLoop {
    pub(crate) app: OpenHarmonyApp,
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    event_loop_proxy: Arc<EventLoopProxy>,
    shared: Arc<SharedWindowState>,
}

impl RootActiveEventLoop for ActiveEventLoop {
    fn create_proxy(&self) -> CoreEventLoopProxy {
        CoreEventLoopProxy::new(self.event_loop_proxy.clone())
    }

    fn create_window(
        &self,
        window_attributes: WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, RequestError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn create_custom_cursor(
        &self,
        _source: CustomCursorSource,
    ) -> Result<CustomCursor, RequestError> {
        Err(NotSupportedError::new("create_custom_cursor is not supported").into())
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
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
}

impl Window {
    pub(crate) fn new(
        el: &ActiveEventLoop,
        _window_attrs: window::WindowAttributes,
    ) -> Result<Self, RequestError> {
        // The primary OHOS window and its XComponent surface are owned by NativeAbility.
        // Attributes that would create or configure an OS window are therefore not applicable.

        Ok(Self {
            app: el.app.clone(),
            shared: el.shared.clone(),
        })
    }

    pub fn request_redraw(&self) {
        // XComponent frame callbacks are owned and scheduled by OHOS. There is no API with which
        // this backend can actively request one, so `RedrawRequested` is emitted only when Ability
        // reports `WindowRedraw`.
    }

    pub fn scale_factor(&self) -> f64 {
        self.shared.lock().scale_factor
    }

    pub fn config(&self) -> Configuration {
        self.app.config()
    }

    pub fn content_rect(&self) -> Rect {
        self.app.content_rect()
    }

    pub fn openharmony_app(&self) -> &OpenHarmonyApp {
        &self.app
    }

    // Allow the usage of HasRawWindowHandle inside this function
    #[allow(deprecated)]
    pub fn raw_window_handle_rwh_06(&self) -> Result<rwh_06::RawWindowHandle, rwh_06::HandleError> {
        if let Some(handle) = self
            .app
            .native_window()
            .and_then(|window| window.raw_window_handle())
        {
            return Ok(handle);
        }

        tracing::error!(
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
        GLOBAL_WINDOW
    }

    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(std::iter::empty())
    }

    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        None
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

    fn set_outer_position(&self, _position: Position) {
        // no effect
    }

    fn surface_size(&self) -> PhysicalSize<u32> {
        self.shared.lock().surface_size
    }

    fn request_surface_size(&self, _size: Size) -> Option<PhysicalSize<u32>> {
        Some(self.surface_size())
    }

    fn outer_size(&self) -> PhysicalSize<u32> {
        self.shared.lock().outer_size
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        safe_area(&self.app)
    }

    fn set_min_surface_size(&self, _: Option<Size>) {}

    fn set_max_surface_size(&self, _: Option<Size>) {}

    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        None
    }

    fn set_surface_resize_increments(&self, _increments: Option<Size>) {}

    fn set_title(&self, _title: &str) {}

    fn set_transparent(&self, _transparent: bool) {}

    fn set_blur(&self, _blur: bool) {}

    fn set_visible(&self, _visibility: bool) {}

    fn is_visible(&self) -> Option<bool> {
        Some(self.shared.lock().visible)
    }

    fn set_resizable(&self, _resizeable: bool) {}

    fn is_resizable(&self) -> bool {
        false
    }

    fn set_enabled_buttons(&self, _buttons: WindowButtons) {}

    fn enabled_buttons(&self) -> WindowButtons {
        WindowButtons::all()
    }

    fn set_minimized(&self, _minimized: bool) {}

    fn is_minimized(&self) -> Option<bool> {
        None
    }

    fn set_maximized(&self, _maximized: bool) {}

    fn is_maximized(&self) -> bool {
        false
    }

    fn set_fullscreen(&self, _monitor: Option<Fullscreen>) {
        warn!("Cannot set fullscreen on OpenHarmony");
    }

    fn fullscreen(&self) -> Option<Fullscreen> {
        None
    }

    fn set_decorations(&self, _decorations: bool) {}

    fn is_decorated(&self) -> bool {
        true
    }

    fn set_window_level(&self, _level: WindowLevel) {}

    fn set_window_icon(&self, _window_icon: Option<winit_core::icon::Icon>) {}

    fn set_ime_cursor_area(&self, _position: Position, _size: Size) {}

    fn request_ime_update(&self, request: ImeRequest) -> Result<(), ImeRequestError> {
        match request {
            ImeRequest::Enable(enable) => {
                let (capabilities, _) = enable.into_raw();
                let mut state = self.shared.lock();
                if state.ime_capabilities.is_some() {
                    return Err(ImeRequestError::AlreadyEnabled);
                }
                state.ime_capabilities = Some(capabilities);
                drop(state);
                self.app.show_keyboard();
            }
            ImeRequest::Update(_) => {
                if self.shared.lock().ime_capabilities.is_none() {
                    return Err(ImeRequestError::NotEnabled);
                }
            }
            ImeRequest::Disable => {
                self.shared.lock().ime_capabilities = None;
                self.app.hide_keyboard();
            }
            _ => return Err(ImeRequestError::NotSupported),
        }

        Ok(())
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        self.shared.lock().ime_capabilities
    }

    fn set_ime_purpose(&self, _purpose: ImePurpose) {}

    fn focus_window(&self) {}

    fn request_user_attention(&self, _request_type: Option<window::UserAttentionType>) {}

    fn set_cursor(&self, _: Cursor) {}

    fn set_cursor_position(&self, _: Position) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_position is not supported").into())
    }

    fn set_cursor_grab(&self, _: CursorGrabMode) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_grab is not supported").into())
    }

    fn set_cursor_visible(&self, _: bool) {}

    fn drag_window(&self) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_window is not supported").into())
    }

    fn drag_resize_window(&self, _direction: ResizeDirection) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_resize_window").into())
    }

    #[inline]
    fn show_window_menu(&self, _position: Position) {}

    fn set_cursor_hittest(&self, _hittest: bool) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_hittest is not supported").into())
    }

    fn set_theme(&self, _theme: Option<Theme>) {}

    fn theme(&self) -> Option<Theme> {
        self.shared.lock().theme
    }

    fn set_content_protected(&self, _protected: bool) {}

    fn has_focus(&self) -> bool {
        self.shared.lock().focused
    }

    fn title(&self) -> String {
        String::new()
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

    for (area_type, area) in app.avoid_areas() {
        if !area.visible || matches!(area_type, AvoidAreaType::Keyboard) {
            continue;
        }
        top = top.max(nonnegative_u32(area.top_rect.height));
        left = left.max(nonnegative_u32(area.left_rect.width));
        bottom = bottom.max(nonnegative_u32(area.bottom_rect.height));
        right = right.max(nonnegative_u32(area.right_rect.width));
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
    fn color_mode_maps_to_winit_theme() {
        assert_eq!(theme(ColorMode::Dark), Some(Theme::Dark));
        assert_eq!(theme(ColorMode::Light), Some(Theme::Light));
        assert_eq!(theme(ColorMode::NoSet), None);
    }
}
