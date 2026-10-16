# winit-ohos

Winit's OpenHarmony backend, driven by `openharmony-ability` lifecycle callbacks.

## Dependency policy

This branch pins `winit-core` and `dpi` to the same richerfu/winit 0.31 beta revision.
Ability, its derive crate and all Ability plugins use the exact crates.io release
`=1.0.0-beta.3`, with published native bindings containing the drag-result and pen-tilt APIs.
The current Ability/N-API toolchain requires Rust 1.88. Keep the Ability family on one
release when updating the dependency graph; `Cargo.lock` records the tested resolution.

`openharmony-ability` and its derive crate are regular dependencies because this repository is an
OHOS-only backend; they are not hidden behind a `target_env = "ohos"` dependency table.

Applications use the Winit facade. Its OHOS target dependency selects `winit-ohos`;
any direct backend dependency used to enable features must resolve to that same package
and source, keeping Winit core types aligned. An
Ability native-module crate still needs the standard direct N-API/Ability dependencies required by
the `#[ability]` macro expansion:

```toml
[dependencies]
napi-ohos = "1.2"
napi-derive-ohos = "1.2"
openharmony-ability = "=1.0.0-beta.3"
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
`request_redraw` enables frame callbacks for each presentable window. By default these
come from XComponent. The optional `native-vsync` feature uses independent NativeVSync
receivers, votes for the display refresh rate (API 20+), and keeps Ability in
`FrameInputDelivery::OnDemand`. Callbacks wake the Ability UI thread; application code
runs there. Both sources stop after 12 idle callbacks and restart on new demand.
NativeVSync creation, refresh-rate voting or frame-request failure selects XComponent
until the surface is recreated. If XComponent registration also fails, a 16 ms timer
delivers one fallback draw per new demand. Hiding or destroying a surface stops its
receiver; callbacks from an old receiver cannot redraw a replacement surface.
Main-thread wakes are coalesced. Lifecycle and input callbacks request a future frame;
presentation happens after event and data-transfer delivery in a frame iteration.
Background and minimized windows retain redraw demand until they become presentable.
OHOS EGL buffer geometry can lag behind XComponent size callbacks, so size changes
and surface creation redraw every display frame for 200 ms.

`ControlFlow::Poll` follows display refreshes. With no presentable window it behaves
like `Wait` until another platform event arrives. `WaitUntil` uses a timer thread
that sleeps until the selected deadline. `ControlFlow` also determines the next
callback's `StartCause`. The platform owns the loop, so `run_app_on_demand` and
`pump_app_events` are unsupported.

A child surface can be destroyed and recreated without closing its window. The
backend calls `can_create_surfaces` again on recreation; applications must recreate
that window's rendering surface because the old native window handle is invalid.
Native child closure delivers `CloseRequested` followed by `Destroyed` once; dropping
a child from the application delivers only `Destroyed`. Root close requests remain
for the application to decide.

Ability `Destroy` releases rendering surfaces, delivers `Destroyed` to children and
then the root, drops the application handler, and requests process termination.
The `#[ability]` initializer runs only once per process; a destroyed event loop
cannot resume if the system rebuilds the Ability in a cached process.

## Platform capabilities

Use Ability `1.0.0-beta.3` and the pinned winit revision together with the matching ArkTS HARs.
`EventLoop` idempotently registers the window, clipboard, URL, files, app-control
and permission facades. `extended-plugins` also registers menus, notifications,
process control, system state and accessibility state. The matching ArkTS factories
must be installed in `NativeAbility.bridgePlugins`. Build those HARs from the same
Ability release source as the Rust dependencies.

Window mutations run asynchronously in per-window order. Native children wait for
surface readiness before their queued configuration runs. Clipboard reads and file
pickers also run asynchronously; never block the Ability UI thread waiting for a
bridge future. Use `task::spawn` and request a repaint when a result arrives.
`events::subscribe` delivers Want, menu, system-state and screen-reader events.
The `file_access` module resolves picker URIs to absolute paths and attempts to persist
and activate grants using `ohos.permission.FILE_ACCESS_PERSIST`. Authorization failures
are logged without discarding valid selections; activation is attempted only for
successfully persisted grants. Native file drags support Copy/Move, owned RGBA previews
and actual OS completion results, retaining their source/data until completion. Pen
pressure, tilt, twist and eraser metadata are forwarded as tablet-tool events. Static
custom cursors and API 22+ 2in1 Ability window icons use typed window-plugin requests.

Applications select this backend through a workspace-root patch while these changes
are unpublished:

```toml
[patch.crates-io]
winit-ohos = { path = "/path/to/winit-ohos" }
```

To enable NativeVSync through Cargo feature unification, add the following alongside
that patch in the application manifest. Keep the backend version aligned with the
Winit facade's dependency:

```toml
[target.'cfg(target_env = "ohos")'.dependencies]
winit-ohos = { version = "=0.31.0-beta.3", features = ["native-vsync"] }
```

Run `cargo check --all-targets --all-features --target aarch64-unknown-linux-ohos`
and the matching Clippy command with `OHOS_NDK_HOME` pointing to the OpenHarmony SDK.
Cross-compilation checks do not replace device tests for pen metadata, drag completion
and accessibility services, or NativeVSync cadence and refresh-rate voting.

The frame scheduler tests replace the NDK/Ability boundary and run on the host without
OHOS libraries:

```sh
rustc --edition=2024 --test --cfg 'feature="native-vsync"' tests/frame_scheduler.rs -o /tmp/winit-ohos-frames
/tmp/winit-ohos-frames
```

## License

[Apache-2.0](./LICENSE-APACHE)/[MIT](./LICENSE-MIT)
