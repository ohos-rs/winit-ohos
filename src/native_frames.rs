//! NativeVSync callbacks only signal the Ability loop; application code and
//! receiver management run on the UI thread.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use ohos_vsync_binding::{OH_NativeVSync_ExpectedRateRange as ExpectedRateRange, Vsync};

use crate::main_wake::MainWake;

#[derive(Debug)]
pub(crate) enum NativeFrameError {
    InvalidRefreshRate(u32),
    Create,
    FrameRate(i32),
    Request(i32),
}

impl std::fmt::Display for NativeFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRefreshRate(hz) => write!(f, "invalid display refresh rate: {hz}"),
            Self::Create => f.write_str("NativeVSync creation failed"),
            Self::FrameRate(code) => write!(f, "NativeVSync frame-rate request failed: {code}"),
            Self::Request(code) => write!(f, "NativeVSync frame request failed: {code}"),
        }
    }
}

impl std::error::Error for NativeFrameError {}

/// Each receiver owns a separate signal, so callbacks queued before surface
/// destruction or idle cleanup cannot deliver a frame to its replacement.
#[derive(Debug, Default)]
struct FrameSignal {
    cancelled: AtomicBool,
    pending: AtomicBool,
}

pub(crate) struct NativeFrames {
    vsync: Vsync,
    signal: Arc<FrameSignal>,
    wake: Arc<MainWake>,
}

impl std::fmt::Debug for NativeFrames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrames")
            .field("signal", &self.signal)
            .finish_non_exhaustive()
    }
}

impl NativeFrames {
    pub(crate) fn new(hz: u32, wake: Arc<MainWake>) -> Result<Self, NativeFrameError> {
        let rate = i32::try_from(hz)
            .ok()
            .filter(|rate| *rate > 0)
            .ok_or(NativeFrameError::InvalidRefreshRate(hz))?;
        let frames = Self {
            vsync: Vsync::try_new("winit-native-render").ok_or(NativeFrameError::Create)?,
            signal: Arc::default(),
            wake,
        };
        let result = frames
            .vsync
            .set_expected_frame_rate_range(ExpectedRateRange {
                min: rate,
                max: rate,
                expected: rate,
            });
        if result != 0 {
            return Err(NativeFrameError::FrameRate(result));
        }
        frames.request()?;
        Ok(frames)
    }

    pub(crate) fn request(&self) -> Result<(), NativeFrameError> {
        let signal = self.signal.clone();
        let wake = self.wake.clone();
        let result = self.vsync.request_frame_once(move |_| {
            if !signal.cancelled.load(Ordering::Acquire) {
                signal.pending.store(true, Ordering::Release);
                wake.notify_frame();
            }
        });
        if result == 0 {
            Ok(())
        } else {
            Err(NativeFrameError::Request(result))
        }
    }

    pub(crate) fn take_callback(&self) -> bool {
        self.signal.pending.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn callback_pending(&self) -> bool {
        self.signal.pending.load(Ordering::Acquire)
    }
}

impl Drop for NativeFrames {
    fn drop(&mut self) {
        self.signal.cancelled.store(true, Ordering::Release);
        // Withdraw the refresh-rate vote before the binding destroys its receiver.
        self.vsync.set_expected_frame_rate_range(ExpectedRateRange {
            min: 0,
            max: 0,
            expected: 0,
        });
    }
}
