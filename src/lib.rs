//! Winit's OpenHarmony backend, driven by
//! [`openharmony-ability`](https://docs.rs/openharmony-ability/).
//!
//! Create the event loop from the Ability initializer and supply its
//! [`OpenHarmonyApp`].
//!
#![cfg(target_env = "ohos")]

mod control_flow;
mod cursor;
pub mod drag_drop;
mod event_loop;
pub mod events;
pub mod file_access;
mod frame;
mod frame_demand;
mod geometry;
mod ime_text;
mod keycodes;
mod main_wake;
mod monitor;
#[cfg(feature = "native-vsync")]
mod native_frames;
pub mod plugins;
mod resize_settle;
mod safe_area;
pub mod task;
mod timer;
mod window_lifecycle;
mod window_status;

use winit_core::event_loop::ActiveEventLoop as CoreActiveEventLoop;
use winit_core::window::Window as CoreWindow;

use self::ability::{Configuration, OpenHarmonyApp, Rect};
pub use crate::event_loop::{
    ActiveEventLoop, EventLoop, EventLoopProxy, PlatformSpecificEventLoopAttributes,
    PlatformSpecificWindowAttributes, Window,
};

/// Additional methods on [`EventLoop`] that are specific to OpenHarmony.
pub trait EventLoopExtOpenHarmony {
    /// Get the [`OpenHarmonyApp`] which was used to create this event loop.
    fn openharmony_app(&self) -> &OpenHarmonyApp;
}

/// Additional methods on [`ActiveEventLoop`] that are specific to OpenHarmony.
pub trait ActiveEventLoopExtOpenHarmony {
    /// Get the [`OpenHarmonyApp`] which was used to create this event loop.
    fn openharmony_app(&self) -> &OpenHarmonyApp;
}

impl ActiveEventLoopExtOpenHarmony for dyn CoreActiveEventLoop + '_ {
    fn openharmony_app(&self) -> &OpenHarmonyApp {
        let event_loop = self
            .cast_ref::<ActiveEventLoop>()
            .expect("ActiveEventLoop is not backed by winit-ohos");
        &event_loop.app
    }
}

/// Additional methods on [`Window`] that are specific to OpenHarmony.
pub trait WindowExtOpenHarmony {
    /// Get the Ability handle that owns this window.
    ///
    /// This can be used with the `plugins::window` facade or plugins owned by the app.
    fn openharmony_app(&self) -> &OpenHarmonyApp;

    /// Consume native IME edits announced by a `WindowEvent::Ime` notification.
    /// Selection and deletion offsets use UTF-16, as required by the OHOS IME.
    ///
    /// The first call opts this window into native edits. Afterwards, OHOS selection
    /// changes and deletions are queued here, and the announcing event is
    /// `Ime::DeleteSurrounding` when the byte counts are known or `Ime::Enabled`
    /// otherwise, so call this on every IME event. Until then, selection changes are
    /// ignored and deletions arrive as `Ime::DeleteSurrounding` (when surrounding text
    /// was reported) or as Backspace/Delete key presses.
    fn take_ime_edits(&self) -> Vec<ImeEdit>;

    fn content_rect(&self) -> Rect;

    fn config(&self) -> Configuration;
}

impl WindowExtOpenHarmony for dyn CoreWindow + '_ {
    fn openharmony_app(&self) -> &OpenHarmonyApp {
        let window = self
            .cast_ref::<Window>()
            .expect("Window is not backed by winit-ohos");
        window.openharmony_app()
    }

    fn take_ime_edits(&self) -> Vec<ImeEdit> {
        self.cast_ref::<Window>()
            .expect("OHOS window")
            .take_ime_edits()
    }

    fn content_rect(&self) -> Rect {
        let window = self
            .cast_ref::<Window>()
            .expect("Window is not backed by winit-ohos");
        window.content_rect()
    }

    fn config(&self) -> Configuration {
        let window = self
            .cast_ref::<Window>()
            .expect("Window is not backed by winit-ohos");
        window.config()
    }
}

impl EventLoopExtOpenHarmony for EventLoop {
    fn openharmony_app(&self) -> &OpenHarmonyApp {
        &self.openharmony_app
    }
}

pub trait EventLoopBuilderExtOpenHarmony {
    /// Associates the [`OpenHarmonyApp`] that was passed to `openharmony-ability::ability` with the event loop
    ///
    /// This must be called on OpenHarmony since the [`OpenHarmonyApp`] is not global state.
    fn with_openharmony_app(&mut self, app: OpenHarmonyApp) -> &mut Self;
}

/// Re-export of the `openharmony-ability` API
///
/// Winit re-exports the `openharmony-ability` API for convenient, version-aligned imports. Native
/// module crates must still include the standard direct Ability and N-API dependencies required by
/// the `#[ability]` macro expansion.
///
///
/// Applications can import both the Ability handle and derive macro from this module:
/// ```rust
/// use winit_ohos::ability::{ability, OpenHarmonyApp};
///
/// #[ability]
/// fn init(app: OpenHarmonyApp) {
///     // ...
/// }
/// ```
pub mod ability {
    #[doc(no_inline)]
    pub use openharmony_ability::*;

    #[doc(no_inline)]
    pub use openharmony_ability_derive::*;
}

/// Edits absent from winit's cross-platform IME event vocabulary.
#[derive(Debug, Clone)]
pub enum ImeEdit {
    Selection { start: usize, end: usize },
    DeleteSurrounding { before: usize, after: usize },
}

mod stylus;
