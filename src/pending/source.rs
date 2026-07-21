//! Pluggable transport sessions for speculative oracle candidates.

use std::{future::Future, pin::Pin, time::SystemTime};

use tokio::{sync::watch, task::JoinHandle};

use super::{PendingOracleEvent, PendingOracleRuntime, PendingOracleSource, PendingOracleSourceId};

/// Owned future returned by a pending candidate source.
pub type PendingOracleSourceFuture =
    Pin<Box<dyn Future<Output = Result<(), PendingOracleSourceError>> + Send + 'static>>;

/// Description of one concrete pending transport connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOracleSourceDescriptor {
    /// Transport family used by the source.
    pub source: PendingOracleSource,
    /// Stable identifier for this provider, relay, or caller-owned stream.
    pub id: PendingOracleSourceId,
}

impl PendingOracleSourceDescriptor {
    /// Construct a source descriptor.
    pub fn new(source: PendingOracleSource, id: PendingOracleSourceId) -> Self {
        Self { source, id }
    }
}

/// Connection state for one concrete pending source.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingOracleSourceState {
    /// A source task has started and is establishing its transport.
    Connecting,
    /// The source reports that its transport is ready.
    Ready,
    /// The source ended with a transport or decoding failure.
    Degraded,
    /// The source stopped normally or was shut down.
    Stopped,
}

/// Latest observable health of one pending source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOracleSourceHealth {
    /// Source family and concrete identifier.
    pub descriptor: PendingOracleSourceDescriptor,
    /// Current connection state.
    pub state: PendingOracleSourceState,
    /// Most recent transport message, including unrelated pending transactions.
    pub last_transport_message_at: Option<SystemTime>,
    /// Most recent candidate submitted to the pending decoder.
    pub last_candidate_at: Option<SystemTime>,
    /// Number of coverage interruptions reported by this source session.
    pub coverage_gap_count: u64,
    /// Most recent source error.
    pub last_error: Option<String>,
}

impl PendingOracleSourceHealth {
    fn connecting(descriptor: PendingOracleSourceDescriptor) -> Self {
        Self {
            descriptor,
            state: PendingOracleSourceState::Connecting,
            last_transport_message_at: None,
            last_candidate_at: None,
            coverage_gap_count: 0,
            last_error: None,
        }
    }
}

/// Explicit interval during which a pending source could not provide coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOracleCoverageGap {
    /// Source whose coverage was interrupted.
    pub descriptor: PendingOracleSourceDescriptor,
    /// Time at which the runtime recorded the gap.
    pub observed_at: SystemTime,
    /// Provider- or relay-facing reason.
    pub reason: String,
}

/// Failure to start or run a pending source.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PendingOracleSourceError {
    /// Pending updates were not enabled on the runtime.
    #[error("pending oracle updates are not enabled on this runtime")]
    PendingUpdatesDisabled,
    /// The source family was not enabled in [`super::PendingOracleConfig`].
    #[error("pending source {transport:?} is not enabled")]
    SourceDisabled {
        /// Disabled transport family.
        transport: PendingOracleSource,
    },
    /// A live source session already owns this stable source id.
    #[error("pending source id {id} is already running")]
    DuplicateSource {
        /// Conflicting stable source identifier.
        id: super::PendingOracleSourceId,
    },
    /// No Tokio runtime was available to own the source task.
    #[error("a Tokio runtime is required to start a pending oracle source")]
    RuntimeUnavailable,
    /// The source connection or protocol failed.
    #[error("pending source transport failed: {0}")]
    Transport(String),
    /// The source task panicked or was cancelled unexpectedly.
    #[error("pending source task failed: {0}")]
    Task(String),
}

/// Asynchronous producer of full pending oracle transaction candidates.
pub trait PendingOracleCandidateSource: Send + 'static {
    /// Return the source family and stable concrete identifier.
    fn descriptor(&self) -> PendingOracleSourceDescriptor;

    /// Run until the source ends or the shutdown receiver changes to `true`.
    fn run(
        self: Box<Self>,
        sink: PendingOracleSourceSink,
        shutdown: watch::Receiver<bool>,
    ) -> PendingOracleSourceFuture;
}

/// Restricted ingestion and health handle supplied to a pending source.
#[derive(Clone, Debug)]
pub struct PendingOracleSourceSink {
    runtime: PendingOracleRuntime,
    descriptor: PendingOracleSourceDescriptor,
}

impl PendingOracleSourceSink {
    pub(crate) fn new(
        runtime: PendingOracleRuntime,
        descriptor: PendingOracleSourceDescriptor,
    ) -> Self {
        Self {
            runtime,
            descriptor,
        }
    }

    /// Report that the underlying source transport is ready.
    pub fn ready(&self) {
        self.runtime
            .update_source_health(&self.descriptor, |health| {
                health.state = PendingOracleSourceState::Ready;
                health.last_error = None;
            });
    }

    /// Record receipt of any source message, including unrelated transactions.
    pub fn transport_message(&self) {
        self.runtime
            .mutate_source_health(&self.descriptor, |health| {
                health.last_transport_message_at = Some(SystemTime::now());
            });
    }

    /// Record that the source submitted a candidate to an oracle adapter.
    pub fn candidate(&self) {
        self.runtime
            .mutate_source_health(&self.descriptor, |health| {
                health.last_candidate_at = Some(SystemTime::now());
            });
    }

    /// Report a transport interval that cannot be reconstructed reliably.
    pub fn coverage_gap(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.runtime
            .update_source_health(&self.descriptor, |health| {
                health.state = PendingOracleSourceState::Degraded;
                health.coverage_gap_count = health.coverage_gap_count.saturating_add(1);
                health.last_error = Some(reason.clone());
            });
        self.runtime
            .publisher()
            .publish(PendingOracleEvent::CoverageGap(PendingOracleCoverageGap {
                descriptor: self.descriptor.clone(),
                observed_at: SystemTime::now(),
                reason,
            }));
    }

    /// Report that a degraded source is attempting to reconnect.
    pub fn reconnecting(&self) {
        self.runtime
            .update_source_health(&self.descriptor, |health| {
                health.state = PendingOracleSourceState::Connecting;
            });
    }

    /// Borrow the pending runtime used to decode and route candidates.
    pub fn runtime(&self) -> &PendingOracleRuntime {
        &self.runtime
    }

    /// Return the source descriptor associated with this sink.
    pub fn descriptor(&self) -> &PendingOracleSourceDescriptor {
        &self.descriptor
    }

    pub(crate) fn connecting(&self) {
        self.runtime
            .insert_source_health(PendingOracleSourceHealth::connecting(
                self.descriptor.clone(),
            ));
    }

    pub(crate) fn stopped(&self) {
        self.runtime
            .update_source_health(&self.descriptor, |health| {
                health.state = PendingOracleSourceState::Stopped;
            });
    }

    pub(crate) fn failed(&self, error: &PendingOracleSourceError) {
        let reason = error.to_string();
        self.runtime
            .update_source_health(&self.descriptor, |health| {
                health.state = PendingOracleSourceState::Degraded;
                health.coverage_gap_count = health.coverage_gap_count.saturating_add(1);
                health.last_error = Some(reason.clone());
            });
        self.runtime
            .publisher()
            .publish(PendingOracleEvent::CoverageGap(PendingOracleCoverageGap {
                descriptor: self.descriptor.clone(),
                observed_at: SystemTime::now(),
                reason,
            }));
    }
}

/// Runtime-owned source task. Dropping the session shuts the source down.
#[derive(Debug)]
pub struct PendingOracleSourceSession {
    descriptor: PendingOracleSourceDescriptor,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), PendingOracleSourceError>>>,
}

impl PendingOracleSourceSession {
    pub(crate) fn new(
        descriptor: PendingOracleSourceDescriptor,
        shutdown: watch::Sender<bool>,
        task: JoinHandle<Result<(), PendingOracleSourceError>>,
    ) -> Self {
        Self {
            descriptor,
            shutdown,
            task: Some(task),
        }
    }

    /// Return the source owned by this session.
    pub fn descriptor(&self) -> &PendingOracleSourceDescriptor {
        &self.descriptor
    }

    /// Request graceful source shutdown.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Wait for the source task to finish.
    pub async fn join(mut self) -> Result<(), PendingOracleSourceError> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|error| PendingOracleSourceError::Task(error.to_string()))?
    }
}

impl Drop for PendingOracleSourceSession {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
