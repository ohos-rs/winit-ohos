//! Snapshot the native display list; no FFI-owned memory escapes the query.
use dpi::{PhysicalPosition, PhysicalSize};
use std::{borrow::Cow, num::NonZeroU32, sync::Arc};
use winit_core::monitor::{MonitorHandle, MonitorHandleProvider, VideoMode};
#[derive(Debug)]
struct Monitor {
    id: u32,
    name: String,
    size: PhysicalSize<u32>,
    scale: f64,
    rate: u32,
}
impl MonitorHandleProvider for Monitor {
    fn id(&self) -> u128 {
        self.id.into()
    }
    fn native_id(&self) -> u64 {
        self.id.into()
    }
    fn name(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(&self.name))
    }
    fn position(&self) -> Option<PhysicalPosition<i32>> {
        None
    }
    fn scale_factor(&self) -> f64 {
        self.scale
    }
    fn current_video_mode(&self) -> Option<VideoMode> {
        Some(VideoMode::new(
            self.size,
            None,
            NonZeroU32::new(self.rate.saturating_mul(1000)),
        ))
    }
    fn video_modes(&self) -> Box<dyn Iterator<Item = VideoMode>> {
        Box::new(self.current_video_mode().into_iter())
    }
}
pub(crate) fn all() -> Vec<MonitorHandle> {
    type CachedMonitors = Option<(std::time::Instant, Vec<MonitorHandle>)>;
    static CACHE: std::sync::OnceLock<std::sync::Mutex<CachedMonitors>> =
        std::sync::OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some((time, monitors)) = &*cache {
        if time.elapsed().as_millis() < 500 {
            return monitors.clone();
        }
    }
    let monitors = query();
    *cache = Some((std::time::Instant::now(), monitors.clone()));
    monitors
}
fn query() -> Vec<MonitorHandle> {
    use ohos_display_sys::*;
    let mut ptr = std::ptr::null_mut();
    // NativeDisplayManager owns both the array and its nested fields. Copy the
    // small immutable snapshot before calling its matching destructor.
    unsafe {
        if OH_NativeDisplayManager_CreateAllDisplays(&mut ptr) != 0 || ptr.is_null() {
            return Vec::new();
        }
        let info = &*ptr;
        let displays = if info.displaysLength <= 128 && !info.displaysInfo.is_null() {
            std::slice::from_raw_parts(info.displaysInfo, info.displaysLength as usize)
        } else {
            &[]
        };
        let result = displays
            .iter()
            .filter(|d| d.isAlive)
            .map(|d| {
                let name: Vec<_> = d
                    .name
                    .iter()
                    .take_while(|c| **c != 0)
                    .map(|c| c.to_ne_bytes()[0])
                    .collect();
                MonitorHandle(Arc::new(Monitor {
                    id: d.id,
                    name: String::from_utf8_lossy(&name).into_owned(),
                    size: PhysicalSize::new(d.width.max(0) as u32, d.height.max(0) as u32),
                    scale: if d.densityPixels > 0.0 {
                        d.densityPixels as f64
                    } else {
                        1.0
                    },
                    rate: d.refreshRate,
                }))
            })
            .collect();
        OH_NativeDisplayManager_DestroyAllDisplays(ptr);
        result
    }
}
pub(crate) fn primary() -> Option<MonitorHandle> {
    let mut id = 0;
    unsafe {
        ohos_display_sys::OH_NativeDisplayManager_GetDefaultDisplayId(&mut id);
    }
    all().into_iter().find(|m| m.native_id() == id)
}
