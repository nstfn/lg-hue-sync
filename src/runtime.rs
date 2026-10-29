use crate::config::TvOffLights;
use crate::web::ControlCommand;
use crate::web::LiveSettings;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{mpsc, oneshot},
    time::{timeout, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineState {
    Running,
    Paused,
}

pub struct CaptureSourcePolicy {
    compatible: Option<bool>,
    source_paused: bool,
    manually_stopped: bool,
}

impl CaptureSourcePolicy {
    pub fn new(compatible: Option<bool>, requested: bool) -> Self {
        Self {
            compatible,
            source_paused: compatible == Some(false) && requested,
            manually_stopped: false,
        }
    }

    pub fn observe(&mut self, compatible: Option<bool>) {
        if compatible.is_some() {
            self.compatible = compatible;
        }
    }

    pub fn manual_request(&mut self, requested: Option<bool>) {
        if let Some(requested) = requested {
            self.manually_stopped = !requested;
            if !requested {
                self.source_paused = false;
            } else if self.blocked() {
                self.source_paused = true;
            }
        }
    }

    pub fn blocked(&self) -> bool {
        self.compatible == Some(false)
    }
    /// Last known source class: `Some(true)` for HDMI, `Some(false)` for a known non-HDMI app.
    pub fn compatible(&self) -> Option<bool> {
        self.compatible
    }
    pub fn manually_stopped(&self) -> bool {
        self.manually_stopped
    }
    pub fn source_paused(&self) -> bool {
        self.source_paused
    }
    pub fn pause_for_source(&mut self) {
        if !self.manually_stopped {
            self.source_paused = true;
        }
    }
    pub fn auto_resume(&self) -> bool {
        self.source_paused && !self.blocked() && !self.manually_stopped
    }
    pub fn resumed(&mut self) {
        self.source_paused = false;
    }
}

/// Outside observations that may re-arm a capture fault; `None` is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExternalState {
    pub tv_active: Option<bool>,
    pub hdmi_source: Option<bool>,
}

/// A fault retries only on a TV wake or a switch to HDMI, never on a steady state, so a
/// persistent fault cannot loop; a manual Stop always wins.
pub fn rearms_capture_fault(
    before: ExternalState,
    after: ExternalState,
    manually_stopped: bool,
) -> bool {
    let woke = before.tv_active == Some(false) && after.tv_active == Some(true);
    let hdmi_selected = before.hdmi_source == Some(false) && after.hdmi_source == Some(true);
    !manually_stopped && (woke || hdmi_selected)
}

#[derive(Default)]
pub struct PendingCommands {
    pub desired_running: Option<bool>,
    pub manual_running: Option<bool>,
    pub restart: bool,
    pub reconfigure: bool,
    pub sync_bridge: bool,
    pub save_config: Vec<oneshot::Sender<Result<(), String>>>,
    pub apply_settings: Option<LiveSettings>,
}

impl PendingCommands {
    pub fn shutdown(&mut self, receiver: mpsc::Receiver<ControlCommand>) {
        // Drop queued and pending reply senders before waiting for HTTP handlers.
        drop(receiver);
        self.save_config.clear();
    }
    pub fn receive(&mut self, receiver: &mut mpsc::Receiver<ControlCommand>) {
        while let Ok(command) = receiver.try_recv() {
            match command {
                ControlCommand::Start => {
                    self.desired_running = Some(true);
                    self.manual_running = Some(true);
                }
                ControlCommand::Stop => {
                    self.desired_running = Some(false);
                    self.manual_running = Some(false);
                }
                ControlCommand::Restart => self.restart = true,
                ControlCommand::Reconfigure => self.reconfigure = true,
                ControlCommand::SyncBridge => self.sync_bridge = true,
                ControlCommand::SaveConfig(reply) => self.save_config.push(reply),
                ControlCommand::ApplySettings(settings) => self.apply_settings = Some(settings),
            }
        }
    }

    pub fn has_management_action(&self) -> bool {
        self.restart
            || self.reconfigure
            || self.sync_bridge
            || !self.save_config.is_empty()
            || self.apply_settings.is_some()
    }

    /// Takes a Stop while running. A Start that lands while running with a faulted capture (a
    /// Start in the same pass as a capture Pause) becomes a capture restart, which clears the
    /// fault, instead of leaving the fault flag set on a running pipeline.
    pub fn take_stop_if_running(&mut self, state: PipelineState, capture_faulted: bool) -> bool {
        if state != PipelineState::Running {
            return false;
        }
        match self.desired_running.take() {
            Some(false) => true,
            Some(true) if capture_faulted => {
                self.restart = true;
                false
            }
            _ => false,
        }
    }

    pub fn resume_fault_for_restart(&mut self, faulted: bool) {
        if faulted && self.restart && self.desired_running != Some(false) {
            self.desired_running = Some(true);
        }
    }

    pub fn capture_restart_cancelled(&self) -> bool {
        self.desired_running == Some(false) || self.reconfigure
    }
}

pub async fn wait_for_capture_retry(
    delay: Duration,
    shutdown: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

pub struct RetryState {
    failures: u32,
    next_attempt: Instant,
    base: Duration,
    max_doublings: u32,
}

impl RetryState {
    pub fn new() -> Self {
        Self::with_backoff(Duration::from_millis(250), 6)
    }

    /// The first failure waits `base * 2`; each further failure doubles the delay, up to
    /// `base * 2^max_doublings`.
    pub fn with_backoff(base: Duration, max_doublings: u32) -> Self {
        Self {
            failures: 0,
            next_attempt: Instant::now(),
            base,
            max_doublings,
        }
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn ready(&self) -> bool {
        Instant::now() >= self.next_attempt
    }

    pub fn success(&mut self) {
        self.failures = 0;
        self.next_attempt = Instant::now();
    }

    pub fn failure(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay = self.base * (1u32 << self.failures.min(self.max_doublings));
        self.next_attempt = Instant::now() + delay;
        delay
    }
}

/// A failed bridge sync leaves Hue blocked and no bridge event may follow, so the sync retries
/// on its own backoff while Hue output is wanted.
pub fn bridge_sync_retry_due(needs_refresh: bool, hue_wanted: bool, retry: &RetryState) -> bool {
    needs_refresh && hue_wanted && retry.ready()
}

/// Lights go off only when the TV is off, Hue sync is on and our application was the active
/// streamer; `Restore` keeps the bridge's own post-stream restore.
pub fn tv_off_lights_due(
    mode: TvOffLights,
    hue_sync_enabled: bool,
    our_stream: bool,
    tv_off: bool,
) -> bool {
    mode == TvOffLights::Off && hue_sync_enabled && our_stream && tv_off
}

/// systemd reports "stopping" only while the whole system shuts down; a service restart or a
/// binary upgrade leaves it "running" or "degraded". Without a state, only a TV already seen in
/// standby counts as off, so a restart of an active TV never turns the lights off.
pub fn sigterm_is_shutdown(system_state: Option<&str>, tv_active: Option<bool>) -> bool {
    match system_state.map(str::trim).unwrap_or_default() {
        "" => tv_active == Some(false),
        state => state == "stopping",
    }
}

/// `systemctl is-system-running`, bounded like `luna::query`: a late child is killed and
/// reaped. The exit status is ignored because every state but "running" exits non-zero.
pub async fn system_state(program: &str, deadline: Duration) -> Option<String> {
    const MAX_REPLY: u64 = 256;
    let mut child = Command::new(program)
        .kill_on_drop(true)
        .arg("is-system-running")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let state = timeout(deadline, async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()?
            .take(MAX_REPLY)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        child.wait().await.ok()?;
        String::from_utf8(bytes)
            .ok()
            .map(|state| state.trim().to_owned())
    })
    .await
    .ok()
    .flatten();
    if state.is_none() {
        let _ = child.kill().await;
    }
    state
}

/// The steps of a shutdown that turns the Hue lights off, injectable for tests.
pub trait ShutdownLightsSteps {
    /// Asks the bridge to stop our Entertainment stream; returns at once.
    fn stop_stream(&mut self);
    /// True once the stop request has completed.
    fn stream_stopped(&self) -> bool;
    /// Turns the area's lights off within its own bound.
    fn lights_off(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownLights {
    Unchanged,
    Off,
    StopTimedOut,
}

/// The bridge restores the lights' pre-stream state when our stream stops, so a lights-off sent
/// while the stream runs is undone. Our stream stops first; the lights go off only once the stop
/// completed within `stop_bound`. A restart (`lights_off` false) runs no step.
pub async fn shutdown_lights_off(
    lights_off: bool,
    stop_bound: Duration,
    steps: &mut impl ShutdownLightsSteps,
) -> ShutdownLights {
    if !lights_off {
        return ShutdownLights::Unchanged;
    }
    steps.stop_stream();
    let deadline = Instant::now() + stop_bound;
    while !steps.stream_stopped() {
        if Instant::now() >= deadline {
            return ShutdownLights::StopTimedOut;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    steps.lights_off();
    ShutdownLights::Off
}

/// The second pass after a standby lights-off runs only in `Off` mode with Hue sync on, while the
/// TV is still seen in standby and our stream stays down. The bridge check that follows refuses
/// any active stream, ours or another application's.
pub fn tv_off_second_pass_due(
    mode: TvOffLights,
    hue_sync_enabled: bool,
    tv_active: Option<bool>,
    our_stream: bool,
) -> bool {
    mode == TvOffLights::Off && hue_sync_enabled && tv_active == Some(false) && !our_stream
}

/// Running loop: what one TV power observation decides. Returns whether the power changed
/// (logged), and the run request it queues: only on a change, with no request already queued
/// and no manual Stop. A first observation counts as a change only for standby while running.
pub fn tv_power_run_request(
    previous: Option<bool>,
    active: bool,
    running: bool,
    queued: Option<bool>,
    manually_stopped: bool,
) -> (bool, Option<bool>) {
    let changed = previous
        .map(|previous| previous != active)
        .unwrap_or(!active && running);
    let request = (changed && queued.is_none() && !manually_stopped).then_some(active);
    (changed, request)
}

/// Paused loop: only a seen standby-to-active wake starts sync, never over a queued request or
/// a manual Stop.
pub fn tv_wake_starts(
    previous: Option<bool>,
    active: bool,
    requested: Option<bool>,
    manually_stopped: bool,
) -> bool {
    requested.is_none() && !manually_stopped && previous == Some(false) && active
}

/// The reason logged when the loop enters its paused state; the first match wins.
pub fn pause_cause(
    capture_faulted: bool,
    source_blocked: bool,
    manually_stopped: bool,
    tv_standby: bool,
    stop_requested: bool,
) -> &'static str {
    if capture_faulted {
        "capture fault"
    } else if source_blocked {
        "unsupported source"
    } else if manually_stopped {
        "manual stop"
    } else if tv_standby {
        "TV standby"
    } else if stop_requested {
        "stop request"
    } else {
        "already paused"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv_power_observations_decide_run_requests_like_the_inline_poll() {
        // Running: a first standby observation stops sync; a first active one changes nothing.
        assert_eq!(
            tv_power_run_request(None, false, true, None, false),
            (true, Some(false))
        );
        assert_eq!(
            tv_power_run_request(None, true, true, None, false),
            (false, None)
        );
        assert_eq!(
            tv_power_run_request(None, false, false, None, false),
            (false, None)
        );
        // Steady states do not re-request; transitions do.
        assert_eq!(
            tv_power_run_request(Some(true), true, true, None, false),
            (false, None)
        );
        assert_eq!(
            tv_power_run_request(Some(true), false, true, None, false),
            (true, Some(false))
        );
        assert_eq!(
            tv_power_run_request(Some(false), true, true, None, false),
            (true, Some(true))
        );
        // A queued request or a manual Stop wins; the change is still logged.
        assert_eq!(
            tv_power_run_request(Some(true), false, true, Some(true), false),
            (true, None)
        );
        assert_eq!(
            tv_power_run_request(Some(true), false, true, None, true),
            (true, None)
        );

        // Paused: only a seen standby-to-active wake starts sync.
        assert!(tv_wake_starts(Some(false), true, None, false));
        assert!(!tv_wake_starts(None, true, None, false));
        assert!(!tv_wake_starts(Some(true), true, None, false));
        assert!(!tv_wake_starts(Some(false), false, None, false));
        assert!(!tv_wake_starts(Some(false), true, Some(false), false));
        assert!(!tv_wake_starts(Some(false), true, None, true));
    }

    #[test]
    fn pause_log_names_the_real_cause() {
        assert_eq!(pause_cause(true, true, true, true, true), "capture fault");
        assert_eq!(
            pause_cause(false, true, false, true, true),
            "unsupported source"
        );
        assert_eq!(pause_cause(false, false, true, true, true), "manual stop");
        assert_eq!(pause_cause(false, false, false, true, true), "TV standby");
        assert_eq!(
            pause_cause(false, false, false, false, true),
            "stop request"
        );
        assert_eq!(
            pause_cause(false, false, false, false, false),
            "already paused"
        );
    }

    #[test]
    fn only_a_system_shutdown_or_a_known_standby_counts_as_tv_off_at_sigterm() {
        assert!(sigterm_is_shutdown(Some("stopping"), Some(true)));
        assert!(sigterm_is_shutdown(Some("stopping\n"), None));
        // A restart or an upgrade while the TV is on keeps the lights.
        for state in ["running", "degraded", "starting", "maintenance"] {
            assert!(!sigterm_is_shutdown(Some(state), Some(true)), "{state}");
            assert!(!sigterm_is_shutdown(Some(state), None), "{state}");
            assert!(!sigterm_is_shutdown(Some(state), Some(false)), "{state}");
        }
        // A failed or timed-out check falls back to the last TV power state.
        for state in [None, Some(""), Some(" \n")] {
            assert!(sigterm_is_shutdown(state, Some(false)));
            assert!(!sigterm_is_shutdown(state, Some(true)));
            assert!(!sigterm_is_shutdown(state, None));
        }
    }

    #[test]
    fn tv_off_lights_are_skipped_on_restore_without_hue_sync_or_without_our_stream() {
        assert!(tv_off_lights_due(TvOffLights::Off, true, true, true));
        assert!(!tv_off_lights_due(TvOffLights::Restore, true, true, true));
        assert!(!tv_off_lights_due(TvOffLights::Off, false, true, true));
        // Another application streams, or we never streamed: the lights are not ours.
        assert!(!tv_off_lights_due(TvOffLights::Off, true, false, true));
        assert!(!tv_off_lights_due(TvOffLights::Off, true, true, false));
    }

    struct RecordedSteps {
        events: Vec<&'static str>,
        stops_after: Option<usize>,
        polls: std::cell::Cell<usize>,
    }

    impl RecordedSteps {
        fn new(stops_after: Option<usize>) -> Self {
            Self {
                events: Vec::new(),
                stops_after,
                polls: std::cell::Cell::new(0),
            }
        }
    }

    impl ShutdownLightsSteps for RecordedSteps {
        fn stop_stream(&mut self) {
            self.events.push("stop");
        }

        fn stream_stopped(&self) -> bool {
            assert_eq!(self.events, ["stop"], "polled before the stop request");
            self.polls.set(self.polls.get() + 1);
            self.stops_after
                .is_some_and(|polls| self.polls.get() > polls)
        }

        fn lights_off(&mut self) {
            self.events.push("off");
        }
    }

    #[tokio::test]
    async fn shutdown_stops_our_stream_before_the_lights_go_off() {
        let mut steps = RecordedSteps::new(Some(3));
        let outcome = shutdown_lights_off(true, Duration::from_secs(4), &mut steps).await;
        assert_eq!(outcome, ShutdownLights::Off);
        assert_eq!(steps.events, ["stop", "off"]);
        assert_eq!(steps.polls.get(), 4);
    }

    #[tokio::test]
    async fn shutdown_skips_lights_off_when_the_stop_times_out() {
        let mut steps = RecordedSteps::new(None);
        let started = Instant::now();
        let outcome = shutdown_lights_off(true, Duration::from_millis(50), &mut steps).await;
        assert_eq!(outcome, ShutdownLights::StopTimedOut);
        assert_eq!(steps.events, ["stop"]);
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn restart_runs_no_shutdown_lights_step() {
        let mut steps = RecordedSteps::new(Some(0));
        let outcome = shutdown_lights_off(false, Duration::from_secs(4), &mut steps).await;
        assert_eq!(outcome, ShutdownLights::Unchanged);
        assert!(steps.events.is_empty());
        assert_eq!(steps.polls.get(), 0);
    }

    #[test]
    fn second_pass_needs_off_mode_a_tv_still_off_and_no_stream_of_ours() {
        use TvOffLights::{Off, Restore};
        assert!(tv_off_second_pass_due(Off, true, Some(false), false));
        // The TV is on again, or its state is unknown.
        assert!(!tv_off_second_pass_due(Off, true, Some(true), false));
        assert!(!tv_off_second_pass_due(Off, true, None, false));
        // Our stream runs or restarts.
        assert!(!tv_off_second_pass_due(Off, true, Some(false), true));
        assert!(!tv_off_second_pass_due(Restore, true, Some(false), false));
        assert!(!tv_off_second_pass_due(Off, false, Some(false), false));
    }

    #[tokio::test]
    async fn system_state_reads_the_reply_and_fails_closed() {
        let echoed = system_state("echo", Duration::from_secs(5)).await;
        assert_eq!(echoed.as_deref(), Some("is-system-running"));
        let missing = system_state("lg-hue-sync-missing", Duration::from_secs(1)).await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn shutdown_interrupts_capture_recovery_before_driver_recreation() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let cancel = shutdown.clone();
        let (may_recreate, ()) = tokio::join!(
            wait_for_capture_retry(Duration::from_secs(30), &shutdown),
            async move {
                tokio::task::yield_now().await;
                cancel.cancel();
            }
        );
        assert!(!may_recreate);
        assert!(!wait_for_capture_retry(Duration::ZERO, &shutdown).await);
        assert!(
            wait_for_capture_retry(Duration::ZERO, &tokio_util::sync::CancellationToken::new())
                .await
        );
    }

    #[tokio::test]
    async fn shutdown_releases_queued_sends_and_all_save_acknowledgements() {
        let (sender, mut receiver) = mpsc::channel(1);
        let (pending_reply, pending_result) = oneshot::channel();
        sender
            .send(ControlCommand::SaveConfig(pending_reply))
            .await
            .unwrap();
        let mut pending = PendingCommands::default();
        pending.receive(&mut receiver);
        let (queued_reply, queued_result) = oneshot::channel();
        sender
            .send(ControlCommand::SaveConfig(queued_reply))
            .await
            .unwrap();
        let blocked = sender.send(ControlCommand::Start);
        tokio::pin!(blocked);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut blocked)
            .await
            .is_err());
        pending.shutdown(receiver);
        assert!(blocked.await.is_err());
        assert!(pending_result.await.is_err());
        assert!(queued_result.await.is_err());
    }

    #[tokio::test]
    async fn latest_start_stop_command_wins_without_losing_other_actions() {
        let (sender, mut receiver) = mpsc::channel(4);
        sender.send(ControlCommand::Start).await.unwrap();
        let (reply, _result) = oneshot::channel();
        sender
            .send(ControlCommand::SaveConfig(reply))
            .await
            .unwrap();
        sender.send(ControlCommand::Stop).await.unwrap();

        let mut pending = PendingCommands::default();
        pending.receive(&mut receiver);

        assert_eq!(pending.desired_running, Some(false));
        assert_eq!(pending.save_config.len(), 1);
        assert!(pending.has_management_action());
    }

    #[tokio::test]
    async fn concurrent_save_requests_keep_each_acknowledgement() {
        let (sender, mut receiver) = mpsc::channel(2);
        let (first_reply, first_result) = oneshot::channel();
        let (second_reply, second_result) = oneshot::channel();
        sender
            .send(ControlCommand::SaveConfig(first_reply))
            .await
            .unwrap();
        sender
            .send(ControlCommand::SaveConfig(second_reply))
            .await
            .unwrap();

        let mut pending = PendingCommands::default();
        pending.receive(&mut receiver);
        assert_eq!(pending.save_config.len(), 2);
        for reply in std::mem::take(&mut pending.save_config) {
            reply.send(Ok(())).unwrap();
        }
        assert_eq!(first_result.await.unwrap(), Ok(()));
        assert_eq!(second_result.await.unwrap(), Ok(()));
    }

    #[test]
    fn start_and_stop_alone_do_not_interrupt_paused_management() {
        let mut pending = PendingCommands {
            desired_running: Some(false),
            ..PendingCommands::default()
        };
        assert!(!pending.has_management_action());
        pending.reconfigure = true;
        assert!(pending.has_management_action());
    }

    #[tokio::test]
    async fn fault_restart_resumes_without_overriding_an_explicit_stop() {
        let mut pending = PendingCommands {
            restart: true,
            ..PendingCommands::default()
        };
        pending.resume_fault_for_restart(false);
        assert_eq!(pending.desired_running, None);
        pending.resume_fault_for_restart(true);
        assert_eq!(pending.desired_running, Some(true));
        assert!(!pending.capture_restart_cancelled());
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(ControlCommand::Stop).await.unwrap();
        // Commands received during the restart delay must cancel capture recreation too.
        pending.receive(&mut receiver);
        assert!(pending.capture_restart_cancelled());
        pending.desired_running = Some(false);
        pending.resume_fault_for_restart(true);
        assert_eq!(pending.desired_running, Some(false));
        assert!(pending.capture_restart_cancelled());
        pending.desired_running = None;
        pending.reconfigure = true;
        assert!(pending.capture_restart_cancelled());
    }

    #[test]
    fn paused_start_request_survives_outer_command_check() {
        let mut pending = PendingCommands {
            desired_running: Some(true),
            ..PendingCommands::default()
        };
        assert!(!pending.take_stop_if_running(PipelineState::Paused, true));
        assert_eq!(pending.desired_running, Some(true));
        assert!(!pending.restart);
    }

    #[test]
    fn running_start_after_capture_pause_restarts_and_clears_the_fault() {
        // A capture Pause leaves a Stop request; a Start in the same pass overwrites it.
        let mut pending = PendingCommands {
            desired_running: Some(true),
            ..PendingCommands::default()
        };
        assert!(!pending.take_stop_if_running(PipelineState::Running, true));
        assert_eq!(pending.desired_running, None);
        assert!(pending.restart, "the restart path clears the capture fault");
        assert!(!pending.capture_restart_cancelled());

        let mut healthy = PendingCommands {
            desired_running: Some(true),
            ..PendingCommands::default()
        };
        assert!(!healthy.take_stop_if_running(PipelineState::Running, false));
        assert!(!healthy.restart);

        let mut stop = PendingCommands {
            desired_running: Some(false),
            ..PendingCommands::default()
        };
        assert!(stop.take_stop_if_running(PipelineState::Running, true));
        assert!(!stop.restart);
    }

    #[test]
    fn capture_fault_rearms_on_tv_wake_or_hdmi_switch_only() {
        let state = |tv_active, hdmi_source| ExternalState {
            tv_active,
            hdmi_source,
        };
        let standby = state(Some(false), Some(true));
        let active = state(Some(true), Some(true));
        let home = state(Some(true), Some(false));
        assert!(rearms_capture_fault(standby, active, false));
        assert!(rearms_capture_fault(home, active, false));
        // A manual Stop is never overridden.
        assert!(!rearms_capture_fault(standby, active, true));
        assert!(!rearms_capture_fault(home, active, true));
        // Steady states and other transitions never re-arm, so a fault cannot loop.
        for steady in [standby, active, home, ExternalState::default()] {
            assert!(!rearms_capture_fault(steady, steady, false));
        }
        assert!(!rearms_capture_fault(active, standby, false));
        assert!(!rearms_capture_fault(active, home, false));
        // Unknown observations are not a transition.
        let unknown = ExternalState::default();
        assert!(!rearms_capture_fault(unknown, active, false));
    }

    #[test]
    fn retry_delay_is_bounded_and_resets() {
        let mut retry = RetryState::new();
        assert_eq!(retry.failure(), Duration::from_millis(500));
        for _ in 0..20 {
            retry.failure();
        }
        assert_eq!(retry.failure(), Duration::from_secs(16));
        retry.success();
        assert!(retry.ready());
        assert_eq!(retry.failure(), Duration::from_millis(500));
    }

    #[test]
    fn failed_bridge_sync_retries_on_a_capped_backoff_only_while_hue_is_wanted() {
        let mut retry = RetryState::with_backoff(Duration::from_secs(1), 5);
        assert!(bridge_sync_retry_due(true, true, &retry));
        assert!(!bridge_sync_retry_due(false, true, &retry));
        assert!(!bridge_sync_retry_due(true, false, &retry));
        let delays: Vec<_> = (0..7).map(|_| retry.failure().as_secs()).collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 32, 32]);
        assert_eq!(retry.failures(), 7);
        // No tight loop: a failure defers the next attempt.
        assert!(!bridge_sync_retry_due(true, true, &retry));
        retry.success();
        assert_eq!(retry.failures(), 0);
        assert!(bridge_sync_retry_due(true, true, &retry));
        assert!(!bridge_sync_retry_due(true, false, &retry));
    }

    #[test]
    fn source_pause_retains_unknown_policy_and_resumes_hdmi_without_rearming_faults() {
        let mut policy = CaptureSourcePolicy::new(Some(false), true);
        assert!(policy.blocked());
        assert!(policy.source_paused());
        assert!(!policy.auto_resume());
        policy.observe(None);
        assert!(policy.blocked());
        policy.observe(Some(true));
        assert!(policy.auto_resume());
        policy.resumed();
        assert!(!policy.auto_resume());
        policy.observe(None);
        assert!(!policy.blocked());
        policy.observe(Some(false));
        assert!(!policy.auto_resume());
        policy.pause_for_source();
        policy.observe(Some(true));
        assert!(policy.auto_resume());
        policy.resumed();
        // A later capture fault or Hue/TV pause is not a source pause.
        assert!(!policy.auto_resume());
    }

    #[tokio::test]
    async fn manual_stop_and_queued_restart_cannot_reinitialize_an_unsupported_source() {
        let mut policy = CaptureSourcePolicy::new(Some(false), true);
        let (sender, mut receiver) = mpsc::channel(2);
        sender.send(ControlCommand::Restart).await.unwrap();
        sender.send(ControlCommand::Stop).await.unwrap();
        let mut pending = PendingCommands::default();
        pending.receive(&mut receiver);
        policy.manual_request(pending.manual_running.take());
        assert!(pending.restart);
        assert!(policy.blocked());
        assert!(policy.manually_stopped());
        assert!(!policy.source_paused());
        assert!(pending.capture_restart_cancelled());
        policy.observe(Some(true));
        assert!(!policy.auto_resume());
        policy.observe(Some(false));
        sender.send(ControlCommand::Start).await.unwrap();
        pending.receive(&mut receiver);
        policy.manual_request(pending.manual_running.take());
        assert!(policy.blocked());
        assert!(policy.source_paused());
        assert!(!policy.manually_stopped());
        assert!(!policy.auto_resume());
        policy.observe(Some(true));
        assert!(policy.auto_resume());
    }
}
