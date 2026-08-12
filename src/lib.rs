//! # OpenHarmony
//!
//! The OpenHarmony backend builds on (and exposes types from) the [`ohos-rs`](https://docs.rs/ohos-rs/) crate.
//!
//! Native OpenHarmony applications need some form of "glue" crate that is responsible
//! for defining the main entry point for your Rust application as well as tracking
//! various life-cycle events and synchronizing with the main thread.
//!
//! Winit uses the [openharmony-ability](https://docs.rs/openharmony-ability/) as a
//! glue crate.
//!
#![cfg(target_env = "ohos")]

mod event_loop;
mod keycodes;
pub mod plugins;

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
    /// This can be used with the optional `plugins::window` facade or plugins owned by the app.
    fn openharmony_app(&self) -> &OpenHarmonyApp;

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
