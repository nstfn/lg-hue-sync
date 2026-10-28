pub mod dile_vt;
pub mod vtcapture;

use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub struct CapturedFrame<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub is_bgra: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("{backend} {operation} returned code {code}")]
pub struct NativeCaptureError {
    pub backend: &'static str,
    pub operation: &'static str,
    pub code: i32,
}

impl NativeCaptureError {
    pub fn is_busy(&self) -> bool {
        // libvtcapture uses 11 for an unavailable scaler; retain its original code.
        matches!(self.code, 11 | -11 | 16 | -16)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("frame unavailable: {0}")]
    Unavailable(#[source] NativeCaptureError),
    #[error("{backend} requires restart: {reason}")]
    RestartRequired {
        backend: &'static str,
        reason: &'static str,
    },
    #[error("invalid capture frame: {0}")]
    InvalidFrame(#[from] anyhow::Error),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    Retry,
    Restart(Duration),
    Pause,
}

const UNAVAILABLE_RETRY_FRAMES: u32 = 4;
const FIRST_FRAME_GRACE: Duration = Duration::from_secs(10);
const HEALTHY_RESET: Duration = Duration::from_secs(30);
const RESTART_BASE_DELAY_MS: u64 = 750;
const RESTART_MAX_DELAY: Duration = Duration::from_secs(16);

/// Transient outages and mode changes restart with a capped backoff and never pause;
/// only an invalid frame pauses, until a manual Start or an outside transition re-arms it.
#[derive(Default)]
pub struct CaptureRecovery {
    unavailable_frames: u32,
    restarts: u32,
    restarts_since_frame: u32,
    healthy_since: Option<Instant>,
    session_frame_seen: bool,
    grace_started: Option<Instant>,
}

impl CaptureRecovery {
    /// Returns the restart count when this frame ends a recovery that needed restarts.
    pub fn frame_received(&mut self, now: Instant) -> Option<u32> {
        self.unavailable_frames = 0;
        self.session_frame_seen = true;
        self.grace_started = None;
        let since = *self.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= HEALTHY_RESET {
            self.restarts = 0;
        }
        let recovered = std::mem::take(&mut self.restarts_since_frame);
        (recovered > 0).then_some(recovered)
    }

    /// A new capture session gets the first-frame grace again; the backoff carries over.
    pub fn session_started(&mut self) {
        self.unavailable_frames = 0;
        self.session_frame_seen = false;
        self.grace_started = None;
    }

    /// Consecutive restarts since the last healthy period, i.e. the latest attempt number.
    pub fn restart_attempt(&self) -> u32 {
        self.restarts
    }

    pub fn failed(&mut self, error: &CaptureError, now: Instant) -> RecoveryAction {
        self.healthy_since = None;
        match error {
            CaptureError::InvalidFrame(_) => return RecoveryAction::Pause,
            CaptureError::Unavailable(_) => {
                // HDMI renegotiation returns null buffers for seconds before the first frame.
                if !self.session_frame_seen {
                    let since = *self.grace_started.get_or_insert(now);
                    if now.saturating_duration_since(since) < FIRST_FRAME_GRACE {
                        return RecoveryAction::Retry;
                    }
                }
                self.unavailable_frames = self.unavailable_frames.saturating_add(1);
                if self.unavailable_frames < UNAVAILABLE_RETRY_FRAMES {
                    return RecoveryAction::Retry;
                }
            }
            CaptureError::RestartRequired { .. } => {}
        }
        self.unavailable_frames = 0;
        let shift = self.restarts.min(5);
        let delay = Duration::from_millis(RESTART_BASE_DELAY_MS << shift).min(RESTART_MAX_DELAY);
        self.restarts = self.restarts.saturating_add(1);
        self.restarts_since_frame = self.restarts_since_frame.saturating_add(1);
        RecoveryAction::Restart(delay)
    }
}

pub trait ScreenCapture {
    fn acquire_frame(&mut self) -> std::result::Result<CapturedFrame<'_>, CaptureError>;
    #[allow(dead_code)]
    fn resolution(&self) -> (u32, u32);
    fn is_real_hardware(&self) -> bool {
        false
    }
}

pub use dile_vt::{detect_source_fps, DileVtCapture, MockCapture};
pub use vtcapture::VtCapture;

/// Drop the native owner before allowing another capture client to acquire the scaler.
pub fn release_capture(capture: &mut Box<dyn ScreenCapture>) {
    let (width, height) = capture.resolution();
    drop(std::mem::replace(
        capture,
        Box::new(MockCapture::new(width, height)),
    ));
}

/// Creates a screen capture instance, prioritizing libvtcapture (webOS 6.x / C1 OLED),
/// falling back to dile_vt, and finally to MockCapture.
pub fn create_capture(width: u32, height: u32) -> Box<dyn ScreenCapture> {
    // 1. Try modern webOS libvtcapture (/usr/lib/libvtcapture.so.1)
    match VtCapture::try_new(width, height) {
        Ok(capture) => {
            info!("Initialized VtCapture driver (/usr/lib/libvtcapture.so.1) successfully.");
            return Box::new(capture);
        }
        Err(e) => {
            if e.downcast_ref::<NativeCaptureError>()
                .is_some_and(NativeCaptureError::is_busy)
            {
                error!(
                    "[-] Capture device is BUSY: /dev/video* hardware scaler is in use by another process (e.g. lg-hue-sync.service). Stop it before starting a manual capture session: systemctl stop lg-hue-sync"
                );
            } else {
                warn!(
                    "VtCapture not available on this platform ({}). Trying DileVtCapture...",
                    e
                );
            }
        }
    }

    // 2. Try legacy libdile_vt (/usr/lib/libdile_vt.so.0)
    match DileVtCapture::try_new(width, height, 0) {
        Ok(capture) => {
            info!("Initialized DileVtCapture driver (/usr/lib/libdile_vt.so.0) successfully.");
            return Box::new(capture);
        }
        Err(e) => {
            if e.downcast_ref::<NativeCaptureError>()
                .is_some_and(NativeCaptureError::is_busy)
            {
                error!(
                    "[-] DileVtCapture device is BUSY: /dev/video* is in use by another process."
                );
            } else {
                warn!(
                    "DileVtCapture not available on this platform ({}). Falling back to MockCapture.",
                    e
                );
            }
        }
    }

    // 3. Fallback to synthetic MockCapture (host macOS, tests, CI)
    Box::new(MockCapture::new(width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_drops_native_owner_once_and_preserves_resolution() {
        struct OwnedCapture(std::rc::Rc<std::cell::Cell<u32>>);
        impl ScreenCapture for OwnedCapture {
            fn acquire_frame(&mut self) -> Result<CapturedFrame<'_>, CaptureError> {
                unreachable!()
            }
            fn resolution(&self) -> (u32, u32) {
                (16, 9)
            }
            fn is_real_hardware(&self) -> bool {
                true
            }
        }
        impl Drop for OwnedCapture {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut capture: Box<dyn ScreenCapture> = Box::new(OwnedCapture(drops.clone()));
        release_capture(&mut capture);
        assert_eq!(drops.get(), 1);
        assert!(!capture.is_real_hardware());
        assert_eq!(capture.resolution(), (16, 9));
        release_capture(&mut capture);
        assert_eq!(drops.get(), 1);
    }

    fn unavailable(code: i32) -> CaptureError {
        CaptureError::Unavailable(NativeCaptureError {
            backend: "libvtcapture",
            operation: "get_buffer",
            code,
        })
    }

    fn transition() -> CaptureError {
        CaptureError::RestartRequired {
            backend: "DILE_VT",
            reason: "format changed",
        }
    }

    fn restart_ms(ms: u64) -> RecoveryAction {
        RecoveryAction::Restart(Duration::from_millis(ms))
    }

    /// A recovery whose session already produced a frame, so the first-frame grace is over.
    fn running_recovery(now: Instant) -> CaptureRecovery {
        let mut recovery = CaptureRecovery::default();
        assert_eq!(recovery.frame_received(now), None);
        recovery
    }

    #[test]
    fn transient_outages_and_repeated_transitions_restart_without_pausing() {
        let now = Instant::now();
        let mut recovery = running_recovery(now);
        for expected_delay in [750, 1500, 3000] {
            for _ in 0..3 {
                assert_eq!(recovery.failed(&unavailable(11), now), RecoveryAction::Retry);
            }
            assert_eq!(recovery.failed(&unavailable(11), now), restart_ms(expected_delay));
            // One valid frame must not erase the backoff for a flapping source.
            recovery.frame_received(now);
        }
        // Former limit: the fourth restart used to pause capture until a manual Start.
        assert_eq!(recovery.failed(&transition(), now), restart_ms(6000));
        recovery.frame_received(now);
        recovery.frame_received(now + Duration::from_secs(30));
        assert_eq!(recovery.failed(&transition(), now), restart_ms(750));
    }

    #[test]
    fn unavailable_frames_never_pause_even_after_100_failures() {
        let now = Instant::now();
        let mut recovery = running_recovery(now);
        let mut restarts = 0;
        for _ in 0..100 {
            for _ in 0..UNAVAILABLE_RETRY_FRAMES {
                match recovery.failed(&unavailable(13), now) {
                    RecoveryAction::Pause => panic!("unavailable frames must not pause capture"),
                    RecoveryAction::Restart(delay) => {
                        assert!(delay <= RESTART_MAX_DELAY);
                        restarts += 1;
                    }
                    RecoveryAction::Retry => {}
                }
            }
        }
        assert_eq!(restarts, 100);
        assert_eq!(recovery.restart_attempt(), 100);
    }

    #[test]
    fn restart_backoff_doubles_up_to_a_16_second_cap() {
        let now = Instant::now();
        let mut recovery = running_recovery(now);
        let expected = [750, 1500, 3000, 6000, 12000, 16000, 16000, 16000];
        for (attempt, delay) in expected.into_iter().enumerate() {
            assert_eq!(recovery.failed(&transition(), now), restart_ms(delay));
            assert_eq!(recovery.restart_attempt(), attempt as u32 + 1);
        }
    }

    #[test]
    fn backoff_resets_only_after_30_seconds_of_healthy_frames() {
        let start = Instant::now();
        let mut recovery = running_recovery(start);
        for delay in [750, 1500, 3000] {
            assert_eq!(recovery.failed(&transition(), start), restart_ms(delay));
        }
        // The first frame after restarts reports the recovery once.
        assert_eq!(recovery.frame_received(start), Some(3));
        assert_eq!(recovery.frame_received(start + Duration::from_secs(29)), None);
        assert_eq!(recovery.failed(&transition(), start), restart_ms(6000));
        assert_eq!(recovery.frame_received(start), Some(1));
        assert_eq!(recovery.frame_received(start + Duration::from_secs(30)), None);
        assert_eq!(recovery.restart_attempt(), 0);
        assert_eq!(recovery.failed(&transition(), start), restart_ms(750));
    }

    #[test]
    fn first_frame_grace_defers_restarts_for_ten_seconds_per_session() {
        let start = Instant::now();
        let mut recovery = CaptureRecovery::default();
        for step in 0..100 {
            let now = start + Duration::from_millis(step * 99);
            assert_eq!(recovery.failed(&unavailable(13), now), RecoveryAction::Retry);
        }
        let expired = start + FIRST_FRAME_GRACE;
        for _ in 0..3 {
            assert_eq!(recovery.failed(&unavailable(13), expired), RecoveryAction::Retry);
        }
        assert_eq!(recovery.failed(&unavailable(13), expired), restart_ms(750));

        // A restarted session gets the grace again but keeps the backoff.
        recovery.session_started();
        let restarted = expired + Duration::from_secs(1);
        for _ in 0..10 {
            assert_eq!(recovery.failed(&unavailable(13), restarted), RecoveryAction::Retry);
        }
        let expired = restarted + FIRST_FRAME_GRACE;
        for _ in 0..3 {
            assert_eq!(recovery.failed(&unavailable(13), expired), RecoveryAction::Retry);
        }
        assert_eq!(recovery.failed(&unavailable(13), expired), restart_ms(1500));

        // After a healthy frame, outages count at once.
        recovery.session_started();
        assert_eq!(recovery.frame_received(expired), Some(2));
        for _ in 0..3 {
            assert_eq!(recovery.failed(&unavailable(13), expired), RecoveryAction::Retry);
        }
        assert_eq!(recovery.failed(&unavailable(13), expired), restart_ms(3000));
    }

    #[test]
    fn successful_frame_clears_a_short_outage_and_invalid_frames_pause_immediately() {
        let now = Instant::now();
        let mut recovery = running_recovery(now);
        for _ in 0..3 {
            assert_eq!(recovery.failed(&unavailable(-5), now), RecoveryAction::Retry);
        }
        recovery.frame_received(now);
        assert_eq!(recovery.failed(&unavailable(-5), now), RecoveryAction::Retry);
        let source = anyhow::Error::new(std::io::Error::from_raw_os_error(22))
            .context("NV12 dimensions invalid");
        let invalid = CaptureError::from(source);
        assert_eq!(recovery.failed(&invalid, now), RecoveryAction::Pause);
        // Other invalid frames still pause, even during the first-frame grace.
        let mut fresh = CaptureRecovery::default();
        let short_chroma = CaptureError::from(anyhow::anyhow!("no NV12 chroma plane"));
        assert_eq!(fresh.failed(&short_chroma, now), RecoveryAction::Pause);
        assert!(std::error::Error::source(&invalid).is_some());
        if let CaptureError::InvalidFrame(source) = invalid {
            assert_eq!(
                source
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(22)
            );
        }
        let wrapped = anyhow::Error::new(NativeCaptureError {
            backend: "libvtcapture",
            operation: "vtCapture_init",
            code: 11,
        })
        .context("capture initialization");
        assert!(wrapped
            .downcast_ref::<NativeCaptureError>()
            .unwrap()
            .is_busy());
        if let CaptureError::Unavailable(native) = unavailable(-5) {
            assert_eq!(native.code, -5);
            assert!(!native.is_busy());
        }
    }

    #[test]
    fn test_create_capture_fallback_to_mock() {
        let mut capture = create_capture(160, 90);
        let frame = capture.acquire_frame().expect("Acquire mock frame failed");
        assert_eq!(frame.width, 160);
        assert_eq!(frame.height, 90);
        assert_eq!(frame.data.len(), 160 * 90 * 4);
    }
}
