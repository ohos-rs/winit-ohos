# winit-ohos

Winit's OpenHarmony backend, driven by `openharmony-ability` lifecycle callbacks.

## Dependency policy

This branch pins `winit-core` and `dpi` to the same richerfu/winit 0.31 beta revision.
Ability and all plugin crates use one matching richerfu/openharmony-ability revision,
which in turn pins the native bindings containing the drag-result and pen-tilt APIs.
The current Ability/N-API toolchain requires Rust 1.88. Keep these revisions aligned
when updating the dependency graph; `Cargo.lock` records the tested resolution.

`openharmony-ability` and its derive crate are regular dependencies because this repository is an
OHOS-only backend; they are not hidden behind a `target_env = "ohos"` dependency table.

Applications should depend on the Winit facade only. Its OHOS target dependency selects
`winit-ohos`, so adding this backend a second time would create conflicting Winit core types. An
Ability native-module crate still needs the standard direct N-API/Ability dependencies required by
the `#[ability]` macro expansion:

```toml
[dependencies]
napi-ohos = "1.2"
napi-derive-ohos = "1.2"
openharmony-ability = { git = "https://github.com/richerfu/openharmony-ability.git", rev = "d8cf549505fca3ea39d4a56cb58334a85a6d1dc6" }
winit = { git = "https://github.com/richerfu/winit.git", rev = "c9ee2bff8a79aacb8068546e7150197693d35a5a" }

[build-dependencies]
napi-build-ohos = "1.2"
```

## Ability entry

Create the backend event loop inside the `#[ability]` initializer. `run_app` registers callbacks
and returns; it must not block the Ability main thread.

```rust,no_run
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop},
    platform::ohos::{
        EventLoopBuilderExtOpenHarmony,
        ability::{OpenHarmonyApp, ability},
    },
    window::WindowId,
};

#[derive(Default)]
struct App;

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, _event_loop: &dyn ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }
}

#[ability]
fn openharmony_app(app: OpenHarmonyApp) {
    EventLoop::builder()
        .with_openharmony_app(app)
        .build()
        .expect("create OHOS event loop")
        .run_app(App)
        .expect("register OHOS event loop");
}
```

The backend translates Ability lifecycle, surface geometry, safe areas, configuration,
XComponent touch/mouse/key input, ArkUI axis/gesture input and IME callbacks into
winit events. `run_app` registers callbacks and returns to the Ability main loop.
`request_redraw` and `EventLoopProxy::wake_up` queue Ability main-thread work;
`ControlFlow` determines the next callback's `StartCause`. The platform owns the
loop, so `run_app_on_demand` and `pump_app_events` are unsupported.

## Platform capabilities

Use the pinned Ability and winit revisions together with the matching ArkTS HARs.
`EventLoop` idempotently registers the window, clipboard, URL, files, app-control
and permission facades. `extended-plugins` also registers menus, notifications,
process control, system state and accessibility state. The matching ArkTS factories
must be installed in `NativeAbility.bridgePlugins`. Build those HARs from the same
Ability commit as the Rust dependencies.

Window mutations run asynchronously in per-window order. Native children wait for
surface readiness before their queued configuration runs. Clipboard reads and file
pickers also run asynchronously; never block the Ability UI thread waiting for a
bridge future. Use `task::spawn` and request a repaint when a result arrives.
`events::subscribe` delivers Want, menu, system-state and screen-reader events.
The `file_access` module persists and activates picker grants before Rust filesystem
access. Native file drags support Copy/Move, owned RGBA previews and actual OS completion
results, retaining their source/data until completion. Pen pressure, tilt, twist
and eraser metadata are forwarded as tablet-tool events. Static custom cursors
and API 22+ 2in1 Ability window icons use typed window-plugin requests.

Applications select this backend through a workspace-root patch while these changes
are unpublished:

```toml
[patch.crates-io]
winit-ohos = { path = "/path/to/winit-ohos" }
```

Run `cargo check --all-targets --all-features --target aarch64-unknown-linux-ohos`
and the matching Clippy command with `OHOS_NDK_HOME` pointing to the OpenHarmony SDK.
Cross-compilation checks do not replace device tests for pen metadata, drag completion
and accessibility services.

## License

[Apache-2.0](./LICENSE-APACHE)/[MIT](./LICENSE-MIT)
