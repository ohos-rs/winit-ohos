//! Optional `openharmony-ability` window capability.
//!
//! Enabling `plugin-window` exposes its Rust facade and makes the backend register it while the
//! `#[ability]` initializer is running. The matching ArkTS HAR must also be installed in the
//! application's `NativeAbility::bridgePlugins` list.

use openharmony_ability::OpenHarmonyApp;

#[cfg(feature = "plugin-window")]
pub use openharmony_ability_plugin_window as window;

pub(crate) fn register_enabled(app: &OpenHarmonyApp) -> openharmony_ability::napi_ohos::Result<()> {
    #[cfg(feature = "plugin-window")]
    if app
        .registered_plugin::<window::WindowBridgePlugin>()?
        .is_none()
    {
        app.register_plugin(window::WindowBridgePlugin)?;
    }

    let _ = app;
    Ok(())
}
