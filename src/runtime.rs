use crate::web::ControlCommand;
use crate::web::LiveSettings;
use std::time::Duration;
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
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

    pub fn take_stop_if_running(&mut self, state: PipelineState) -> bool {
        state == PipelineState::Running && self.desired_running.take() == Some(false)
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!pending.take_stop_if_running(PipelineState::Paused));
        assert_eq!(pending.desired_running, Some(true));
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
