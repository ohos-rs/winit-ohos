# winit-ohos

Winit's OpenHarmony backend, driven by `openharmony-ability` lifecycle callbacks.

## Dependency policy

This crate intentionally tracks the OHOS-enabled Winit `master` branch instead of a released Winit
version. The current Ability/N-API toolchain requires Rust 1.88. `Cargo.lock` pins the exact branch
commit that was compiled and tested. Run
`cargo update -p winit-core -p dpi` when intentionally moving to a newer Winit commit.

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
openharmony-ability = "1.0.0-beta.2"
winit = { git = "https://github.com/richerfu/winit.git", branch = "master" }

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

The backend translates Ability 1.0 lifecycle, surface, content-rect, avoid-area, configuration,
raw XComponent touch/mouse/key input, ArkUI axis and gesture input, and IME callbacks into current
`winit-core` events. The loop remains driven by OHOS: `RedrawRequested` is emitted only for the
system's `WindowRedraw` callback, and
`Window::request_redraw` cannot actively schedule a frame. `EventLoopProxy::wake_up` asks Ability
to queue a task; `proxy_wake_up` is delivered only after OHOS executes that task on its main
thread. Likewise, `ControlFlow` affects the `StartCause` of the next system callback but cannot
independently start polling or a deadline timer. `run_app_on_demand` and `pump_app_events` are
intentionally not supported because the Ability lifecycle owns the main loop.

## Ability window plugin

Only the window capability is integrated because it belongs to this backend's domain. The
`plugin-window` feature is disabled by default; application-level plugins such as files,
permissions, resources, URLs, webviews and app control remain the application's responsibility.
Enable the window facade explicitly when needed:

```toml
[dependencies]
winit = { git = "https://github.com/richerfu/winit.git", branch = "master", features = ["ohos-window-plugin"] }
```

Constructing `EventLoop` automatically and idempotently registers the enabled Rust facade. It also
requires the matching `@ohos-rs/ability-plugin-window` ArkTS HAR and factory in
`NativeAbility::bridgePlugins`; enabling only the Cargo feature is not sufficient:

```ts
import { LazyPlugin, NativeAbility } from "@ohos-rs/ability";
import { WindowPlugin } from "@ohos-rs/ability-plugin-window";

export default class EntryAbility extends NativeAbility {
  public moduleName: string = "my_native_module";
  public bridgePlugins = [
    new LazyPlugin(() => new WindowPlugin()),
  ];
}
```

The window plugin is exposed as an asynchronous capability facade for avoid-area queries and OS
sub-window operations. It is not used to fake synchronous Winit window setters. Access the same
`OpenHarmonyApp` through `EventLoopExtOpenHarmony`, `ActiveEventLoopExtOpenHarmony`, or
`WindowExtOpenHarmony`, then use `plugins::window`'s extension trait. Other Ability plugins should
be declared and registered directly by the application.

## License

[Apache-2.0](./LICENSE-APACHE)/[MIT](./LICENSE-MIT)
