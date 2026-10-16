# winit-ohos

Winit's OpenHarmony backend, driven by `openharmony-ability` lifecycle callbacks.

## Setup

Requires Rust 1.88 or newer and the OpenHarmony SDK. Applications use the Winit facade;
native module crates also declare the Ability and N-API dependencies required by `#[ability]`.
Keep Ability, its derive crate and plugins aligned at `=1.0.0-beta.3`.

```toml
[dependencies]
napi-ohos = "1.2"
napi-derive-ohos = "1.2"
openharmony-ability = "=1.0.0-beta.3"
winit = { git = "https://github.com/richerfu/winit.git", rev = "c9ee2bff8a79aacb8068546e7150197693d35a5a" }

[build-dependencies]
napi-build-ohos = "1.2"

[patch.crates-io]
winit-ohos = { path = "/path/to/winit-ohos" }
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

The platform owns the event loop; `run_app_on_demand` and `pump_app_events` are unsupported.
`ControlFlow::Poll` follows display refreshes and waits when no window is presentable.
Recreate rendering surfaces when `can_create_surfaces` is called after surface recreation;
the previous native window handle is no longer valid.

## Features

- `native-vsync`: use NativeVSync (API 20+) for frame callbacks, with XComponent fallback.
  Application callbacks stay on the Ability UI thread; frame delivery stops when idle or hidden.
- `extended-plugins`: add menus, notifications, process control, system state and accessibility.

To enable a backend feature, add a direct dependency that resolves to the same package
and source selected by the Winit facade and the patch above:

```toml
[target.'cfg(target_env = "ohos")'.dependencies]
winit-ohos = { version = "=0.31.0-beta.3", features = ["native-vsync"] }
```

## Platform plugins

Install the matching ArkTS HAR factories in `NativeAbility.bridgePlugins`, using the
same Ability release as the Rust dependencies. The event loop registers the enabled
Rust plugin facades automatically.

Window commands, clipboard reads and file pickers are asynchronous. Use `task::spawn`
and request a repaint when a result arrives; keep the Ability UI thread free of blocking
bridge calls. `events::subscribe` exposes platform events without a Winit equivalent.

## Development

Set `OHOS_NDK_HOME` to the OpenHarmony SDK, then run:

```sh
cargo check --all-targets --all-features --target aarch64-unknown-linux-ohos
cargo clippy --all-targets --all-features --target aarch64-unknown-linux-ohos -- -D warnings
cargo fmt --check
```

Run frame scheduler tests on the host without OHOS libraries:

```sh
rustc --edition=2024 --test --cfg 'feature="native-vsync"' tests/frame_scheduler.rs -o /tmp/winit-ohos-frames
/tmp/winit-ohos-frames
```

## License

[Apache-2.0](./LICENSE-APACHE)/[MIT](./LICENSE-MIT)
