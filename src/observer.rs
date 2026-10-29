//! Luna state polling off the frame loop.
//!
//! Each `luna-send` query can take up to its 1.5 s deadline. One background task runs the
//! foreground-source, TV-power and source-fps queries every 2 s and publishes each round
//! through a `watch` channel; the frame loop only takes the newest round, never waiting.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::time::Instant;

/// The cadence the frame loop used for its inline queries.
pub const POLL_PERIOD: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub enum PowerObservation {
    /// `Some(true)` active, `Some(false)` standby, `None` an unknown state.
    State(Option<bool>),
    /// The query failed; the message is for the rate-limited warning.
    Unavailable(String),
}

/// One polling round.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observations {
    /// When the round started; `None` only for the channel's initial, unpublished value.
    pub started: Option<Instant>,
    /// HDMI compatibility of the foreground app (`CaptureSource::compatible`).
    pub source: Option<bool>,
    /// `None` when TV power following was off for this round.
    pub power: Option<PowerObservation>,
    /// `None` when not polled or unavailable.
    pub source_fps: Option<f64>,
}

impl Observations {
    /// The source value, unless a direct query that started after this round supersedes it.
    pub fn source_since(&self, direct_query: Option<Instant>) -> Option<Option<bool>> {
        self.started
            .filter(|started| direct_query.is_none_or(|direct| *started >= direct))
            .map(|_| self.source)
    }
}

type Query<T> = Box<dyn Fn() -> Pin<Box<dyn Future<Output = T> + Send>> + Send + Sync>;

pub struct LunaQueries {
    pub source: Query<Option<bool>>,
    pub power: Query<anyhow::Result<Option<bool>>>,
    pub source_fps: Query<Option<f64>>,
}

impl LunaQueries {
    /// The TV's Luna services through the bounded shared transport.
    pub fn tv() -> Self {
        Self {
            source: Box::new(|| {
                Box::pin(async { crate::capture_source::current_source().await.compatible() })
            }),
            power: Box::new(|| Box::pin(crate::tv_power::is_active())),
            source_fps: Box::new(|| Box::pin(crate::capture::detect_source_fps())),
        }
    }
}

struct Control {
    power: AtomicBool,
    source_fps: AtomicBool,
    wake: Notify,
}

pub struct LunaObserver {
    receiver: watch::Receiver<Observations>,
    control: Arc<Control>,
    task: tokio::task::JoinHandle<()>,
}

impl LunaObserver {
    /// Starts the first round at once.
    pub fn spawn(queries: LunaQueries, period: Duration, power: bool, source_fps: bool) -> Self {
        let (sender, receiver) = watch::channel(Observations::default());
        let control = Arc::new(Control {
            power: AtomicBool::new(power),
            source_fps: AtomicBool::new(source_fps),
            wake: Notify::new(),
        });
        let task = tokio::spawn(poll(queries, period, control.clone(), sender));
        Self {
            receiver,
            control,
            task,
        }
    }

    /// The newest round not taken yet. Older unread rounds are dropped.
    pub fn take(&mut self) -> Option<Observations> {
        match self.receiver.has_changed() {
            Ok(true) => Some(self.receiver.borrow_and_update().clone()),
            _ => None,
        }
    }

    /// Takes effect from the next round's power query.
    pub fn set_power(&self, enabled: bool) {
        self.control.power.store(enabled, Ordering::Relaxed);
    }

    /// Source fps is polled only while auto fps and the pipeline both run.
    pub fn set_source_fps(&self, enabled: bool) {
        self.control.source_fps.store(enabled, Ordering::Relaxed);
    }

    /// Starts the next round now, or right after the current one, instead of at the period.
    pub fn poll_now(&self) {
        self.control.wake.notify_one();
    }
}

impl Drop for LunaObserver {
    fn drop(&mut self) {
        // The transport kills and reaps an in-flight `luna-send` when its future drops.
        self.task.abort();
    }
}

async fn poll(
    queries: LunaQueries,
    period: Duration,
    control: Arc<Control>,
    sender: watch::Sender<Observations>,
) {
    loop {
        let started = Instant::now();
        let source = (queries.source)().await;
        let power = if control.power.load(Ordering::Relaxed) {
            Some(match (queries.power)().await {
                Ok(state) => PowerObservation::State(state),
                Err(error) => PowerObservation::Unavailable(error.to_string()),
            })
        } else {
            None
        };
        let source_fps = if control.source_fps.load(Ordering::Relaxed) {
            (queries.source_fps)().await
        } else {
            None
        };
        let round = Observations {
            started: Some(started),
            source,
            power,
            source_fps,
        };
        if sender.send(round).is_err() {
            return;
        }
        // Start to start, as the inline polls were: a slow round is followed at once.
        tokio::select! {
            _ = tokio::time::sleep_until(started + period) => {}
            _ = control.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    fn queries(power_calls: Arc<AtomicU32>, stall_power: bool) -> LunaQueries {
        LunaQueries {
            source: Box::new(|| Box::pin(async { Some(true) })),
            power: Box::new(move || {
                let call = power_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if stall_power {
                        std::future::pending::<()>().await;
                    }
                    match call % 3 {
                        0 => Ok(Some(true)),
                        1 => Ok(Some(false)),
                        _ => Err(anyhow::anyhow!("webOS TV power query failed")),
                    }
                })
            }),
            source_fps: Box::new(|| Box::pin(async { Some(24.0) })),
        }
    }

    async fn next(observer: &mut LunaObserver) -> Observations {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(round) = observer.take() {
                    return round;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("no observation round")
    }

    #[tokio::test]
    async fn rounds_publish_only_the_enabled_queries_and_the_newest_values() {
        let calls = Arc::new(AtomicU32::new(0));
        let mut observer = LunaObserver::spawn(
            queries(calls.clone(), false),
            Duration::from_secs(60),
            false,
            false,
        );
        let first = next(&mut observer).await;
        assert_eq!(first.source, Some(true));
        assert_eq!((first.power, first.source_fps), (None, None));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(observer.take().is_none(), "a round is taken once");

        observer.set_power(true);
        observer.set_source_fps(true);
        observer.poll_now();
        let second = next(&mut observer).await;
        assert_eq!(second.power, Some(PowerObservation::State(Some(true))));
        assert_eq!(second.source_fps, Some(24.0));
        assert!(second.started > first.started);

        observer.poll_now();
        assert_eq!(
            next(&mut observer).await.power,
            Some(PowerObservation::State(Some(false)))
        );
        observer.poll_now();
        assert_eq!(
            next(&mut observer).await.power,
            Some(PowerObservation::Unavailable(
                "webOS TV power query failed".into()
            ))
        );
    }

    #[tokio::test]
    async fn a_stalled_query_never_blocks_the_reader() {
        let calls = Arc::new(AtomicU32::new(0));
        let mut observer = LunaObserver::spawn(
            queries(calls.clone(), true),
            Duration::from_millis(5),
            true,
            false,
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            let polled = Instant::now();
            assert!(observer.take().is_none());
            assert!(polled.elapsed() < Duration::from_millis(50));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one query stays in flight");
    }

    #[test]
    fn a_newer_direct_source_query_supersedes_an_older_round() {
        let started = Instant::now();
        let round = Observations {
            started: Some(started),
            source: Some(false),
            ..Observations::default()
        };
        assert_eq!(round.source_since(None), Some(Some(false)));
        assert_eq!(round.source_since(Some(started)), Some(Some(false)));
        assert_eq!(
            round.source_since(Some(started + Duration::from_millis(1))),
            None
        );
        assert_eq!(Observations::default().source_since(None), None);
    }
}
