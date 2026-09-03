//! Per-worker in-flight admission control with bounded queuing.
//!
//! The existing global concurrency limiter releases its token as soon as a
//! streaming handler returns response headers, so it cannot protect workers
//! from accumulating long-running decode streams. This module adds a
//! per-worker gate that is acquired *after* a worker is selected (so
//! consistent-hash affinity is preserved) and held until the worker stream
//! really finishes.
//!
//! Queue waits are bounded by `worker_queue_size` (overflow returns 429).
//! A `queue_timeout` of zero means queued requests wait indefinitely (the
//! recommended setting for multi-turn agent traffic, so the router never
//! cuts the client off with a 408 while a worker is merely busy). A positive
//! `queue_timeout` remains available as a safety net and returns 408 when
//! the wait is exceeded.

use crate::metrics::RouterMetrics;
use dashmap::DashMap;
use http::StatusCode;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tracing::debug;

/// Admission configuration for per-worker request gates.
#[derive(Debug, Clone)]
pub struct WorkerAdmissionConfig {
    /// Maximum number of requests that may be in flight per worker.
    /// `None` (the default) disables the per-worker gate.
    pub max_concurrent_requests_per_worker: Option<usize>,
    /// Maximum number of requests waiting in each worker's queue.
    /// `0` means no queue and requests are rejected immediately when full.
    pub worker_queue_size: usize,
    /// Maximum time a request can wait in a worker queue.
    pub queue_timeout: Duration,
}

impl Default for WorkerAdmissionConfig {
    fn default() -> Self {
        Self {
            max_concurrent_requests_per_worker: None,
            worker_queue_size: 100,
            queue_timeout: Duration::ZERO, // 0 = wait indefinitely
        }
    }
}

/// Reasons a request was not admitted to a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionReject {
    /// The worker queue is full (or queuing is disabled).
    QueueFull,
    /// The request timed out while waiting for a worker slot.
    QueueTimeout,
}

impl AdmissionReject {
    pub fn status_code(self) -> StatusCode {
        match self {
            Self::QueueFull => StatusCode::TOO_MANY_REQUESTS,
            // Only produced when an explicit positive queue_timeout is set;
            // with the default (0) queued requests wait indefinitely.
            Self::QueueTimeout => StatusCode::REQUEST_TIMEOUT,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::QueueTimeout => "queue_timeout",
        }
    }
}

/// Snapshot of the per-worker admission gate used by load-aware routing.
///
/// `enabled` is false when the per-worker gate is not configured; in that
/// case `inflight`/`queued`/`max_inflight` are all zero and policies should
/// fall back to the legacy `worker.load()` counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerAdmissionStats {
    /// Whether the per-worker gate is enabled for this worker.
    pub enabled: bool,
    /// Requests currently being served (held until body/SSE stream ends).
    pub inflight: usize,
    /// Requests currently waiting in this worker's queue.
    pub queued: usize,
    /// `max_concurrent_requests_per_worker` for this worker.
    pub max_inflight: usize,
}

/// Shared per-worker state.
struct WorkerLimits {
    worker_url: String,
    max_inflight: usize,
    inflight: Arc<Semaphore>,
    queue_slots: Option<Arc<Semaphore>>,
    queue_timeout: Duration,
    queued: AtomicUsize,
}

impl WorkerLimits {
    fn new(
        worker_url: String,
        max_inflight: usize,
        queue_size: usize,
        queue_timeout: Duration,
    ) -> Self {
        let inflight = Arc::new(Semaphore::new(max_inflight.max(1)));
        let queue_slots = if queue_size > 0 {
            Some(Arc::new(Semaphore::new(queue_size)))
        } else {
            None
        };
        Self {
            worker_url,
            max_inflight,
            inflight,
            queue_slots,
            queue_timeout,
            queued: AtomicUsize::new(0),
        }
    }

    async fn acquire(self: &Arc<Self>) -> Result<WorkerSlotPermit, AdmissionReject> {
        // Fast path: slot is available right now.
        if let Ok(permit) = self.inflight.clone().try_acquire_owned() {
            self.update_inflight_metric();
            return Ok(WorkerSlotPermit {
                limits: Some(Arc::clone(self)),
                _inflight: Some(permit),
            });
        }

        let queue_slots = match &self.queue_slots {
            Some(slots) => Arc::clone(slots),
            None => return Err(AdmissionReject::QueueFull),
        };

        // The queue-slot permit bounds the number of tasks waiting on the
        // inflight semaphore for this worker.
        let queue_permit = queue_slots
            .try_acquire_owned()
            .map_err(|_| AdmissionReject::QueueFull)?;
        let _queue_guard = QueuedGuard::new(Arc::clone(self));

        let started = Instant::now();
        // queue_timeout == 0 means the router holds the connection and waits
        // for a free slot instead of failing the client with a 408. The only
        // ways out are acquiring a slot, queue overflow, or the client
        // disconnecting (which cancels this task and drops the queue permit).
        let wait_result = if self.queue_timeout.is_zero() {
            Ok(self.inflight.clone().acquire_owned().await)
        } else {
            timeout(self.queue_timeout, self.inflight.clone().acquire_owned()).await
        };
        drop(queue_permit);

        match wait_result {
            Ok(Ok(permit)) => {
                debug!(
                    worker = %self.worker_url,
                    wait_ms = started.elapsed().as_millis(),
                    "per-worker admission: acquired slot after waiting"
                );
                self.update_inflight_metric();
                Ok(WorkerSlotPermit {
                    limits: Some(Arc::clone(self)),
                    _inflight: Some(permit),
                })
            }
            Ok(Err(_)) => {
                // The semaphore was closed; this is not expected at runtime.
                Err(AdmissionReject::QueueFull)
            }
            Err(_) => {
                debug!(
                    worker = %self.worker_url,
                    wait_ms = started.elapsed().as_millis(),
                    "per-worker admission: queue wait timed out"
                );
                Err(AdmissionReject::QueueTimeout)
            }
        }
    }

    fn update_inflight_metric(&self) {
        let used = self
            .max_inflight
            .saturating_sub(self.inflight.available_permits());
        RouterMetrics::set_worker_inflight_requests(&self.worker_url, used);
    }

    fn update_queued_metric(&self) {
        RouterMetrics::set_worker_queued_requests(
            &self.worker_url,
            self.queued.load(Ordering::Relaxed),
        );
    }

    fn stats(&self) -> WorkerAdmissionStats {
        WorkerAdmissionStats {
            enabled: true,
            inflight: self
                .max_inflight
                .saturating_sub(self.inflight.available_permits()),
            queued: self.queued.load(Ordering::Relaxed),
            max_inflight: self.max_inflight,
        }
    }
}

/// RAII guard that keeps the queued gauge accurate even when a waiter is
/// cancelled or times out.
struct QueuedGuard {
    limits: Arc<WorkerLimits>,
}

impl QueuedGuard {
    fn new(limits: Arc<WorkerLimits>) -> Self {
        limits.queued.fetch_add(1, Ordering::Relaxed);
        limits.update_queued_metric();
        Self { limits }
    }
}

impl Drop for QueuedGuard {
    fn drop(&mut self) {
        self.limits.queued.fetch_sub(1, Ordering::Relaxed);
        self.limits.update_queued_metric();
    }
}

/// A held per-worker in-flight slot. Dropping the permit returns the slot.
pub struct WorkerSlotPermit {
    limits: Option<Arc<WorkerLimits>>,
    _inflight: Option<OwnedSemaphorePermit>,
}

impl Drop for WorkerSlotPermit {
    fn drop(&mut self) {
        // Release the semaphore permit first, then refresh the gauge.
        self._inflight = None;
        if let Some(limits) = &self.limits {
            limits.update_inflight_metric();
        }
    }
}

/// Per-worker admission gate. Entry points are worker URLs so the same
/// physical worker can be shared by multiple routers in IGW mode.
#[derive(Default, Clone)]
pub struct WorkerAdmission {
    config: WorkerAdmissionConfig,
    limits: Arc<DashMap<String, Arc<WorkerLimits>>>,
}

impl fmt::Debug for WorkerAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerAdmission")
            .field("config", &self.config)
            .field("active_worker_limits", &self.limits.len())
            .finish()
    }
}

impl WorkerAdmission {
    pub fn new(config: WorkerAdmissionConfig) -> Self {
        Self {
            config,
            limits: Arc::new(DashMap::new()),
        }
    }

    /// Acquire a per-worker slot. Returns `Ok(None)` when the gate is
    /// disabled, so callers can treat it as a no-op.
    pub async fn acquire(
        &self,
        worker_url: &str,
    ) -> Result<Option<WorkerSlotPermit>, AdmissionReject> {
        let max_inflight = match self.config.max_concurrent_requests_per_worker {
            Some(max) if max > 0 => max,
            _ => return Ok(None),
        };

        let limits = if let Some(existing) = self.limits.get(worker_url) {
            Arc::clone(existing.value())
        } else {
            Arc::clone(
                self.limits
                    .entry(worker_url.to_string())
                    .or_insert_with(|| {
                        Arc::new(WorkerLimits::new(
                            worker_url.to_string(),
                            max_inflight,
                            self.config.worker_queue_size,
                            self.config.queue_timeout,
                        ))
                    })
                    .value(),
            )
        };

        limits.acquire().await.map(Some)
    }

    /// Snapshot the admission state for a worker URL.
    ///
    /// Returns an empty, disabled snapshot when the gate itself is disabled.
    /// When the gate is enabled but the worker has not been seen yet, returns
    /// an enabled snapshot with zero load so policies can route to it.
    pub fn stats(&self, worker_url: &str) -> WorkerAdmissionStats {
        let max_inflight = match self.config.max_concurrent_requests_per_worker {
            Some(max) if max > 0 => max,
            _ => return WorkerAdmissionStats::default(),
        };

        match self.limits.get(worker_url) {
            Some(limits) => limits.stats(),
            None => WorkerAdmissionStats {
                enabled: true,
                max_inflight,
                ..WorkerAdmissionStats::default()
            },
        }
    }

    /// Drop the per-worker limit state when a worker is removed.
    pub fn remove_worker(&self, worker_url: &str) {
        self.limits.remove(worker_url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config(max: Option<usize>, queue_size: usize, timeout_ms: u64) -> WorkerAdmissionConfig {
        WorkerAdmissionConfig {
            max_concurrent_requests_per_worker: max,
            worker_queue_size: queue_size,
            queue_timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[tokio::test]
    async fn disabled_gate_is_noop() {
        let admission = WorkerAdmission::new(config(None, 1, 1000));
        let permit = admission.acquire("http://w1:8000").await.unwrap();
        assert!(permit.is_none());
        assert_eq!(
            admission.stats("http://w1:8000"),
            WorkerAdmissionStats::default()
        );
    }

    #[tokio::test]
    async fn releases_slot_to_waiting_request() {
        let admission = WorkerAdmission::new(config(Some(1), 2, 1000));
        let first = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("first permit");

        let admission_clone = admission.clone();
        let second = tokio::spawn(async move {
            admission_clone
                .acquire("http://w1:8000")
                .await
                .unwrap()
                .expect("second permit should wait then succeed")
        });

        // Let the second task enqueue before releasing the first slot.
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(first);

        second.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_when_worker_queue_full() {
        let admission = WorkerAdmission::new(config(Some(1), 1, 5000));
        let first = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("first permit");

        let admission_clone = admission.clone();
        let second = tokio::spawn(async move {
            admission_clone
                .acquire("http://w1:8000")
                .await
                .expect("second should be queued")
        });

        // Ensure the second request owns the single queue slot.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let third = admission.acquire("http://w1:8000").await;
        assert!(matches!(third, Err(AdmissionReject::QueueFull)));

        drop(first);
        second.await.unwrap();
    }

    #[tokio::test]
    async fn times_out_while_waiting_for_slot() {
        let admission = WorkerAdmission::new(config(Some(1), 1, 50));
        let _first = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("first permit");

        let result = admission.acquire("http://w1:8000").await;
        assert!(matches!(result, Err(AdmissionReject::QueueTimeout)));
    }

    #[tokio::test]
    async fn zero_timeout_waits_indefinitely_for_slot() {
        let admission = WorkerAdmission::new(config(Some(1), 1, 0));
        let first = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("first permit");

        let admission_clone = admission.clone();
        let second = tokio::spawn(async move {
            admission_clone
                .acquire("http://w1:8000")
                .await
                .unwrap()
                .expect("second permit should wait then succeed")
        });

        // A zero timeout must not reject the queued request; it waits until
        // the first slot is released.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!second.is_finished());

        drop(first);
        second.await.unwrap();
    }

    #[tokio::test]
    async fn stats_reflect_inflight_and_queued_requests() {
        let admission = WorkerAdmission::new(config(Some(1), 1, 5000));
        let expected_idle = WorkerAdmissionStats {
            enabled: true,
            inflight: 0,
            queued: 0,
            max_inflight: 1,
        };
        assert_eq!(admission.stats("http://w1:8000"), expected_idle);

        let first = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("first permit");
        let after_first = admission.stats("http://w1:8000");
        assert_eq!(after_first.inflight, 1);
        assert_eq!(after_first.queued, 0);

        let admission_clone = admission.clone();
        let second = tokio::spawn(async move {
            admission_clone
                .acquire("http://w1:8000")
                .await
                .unwrap()
                .expect("second permit")
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let queued = admission.stats("http://w1:8000");
        assert_eq!(queued.inflight, 1);
        assert_eq!(queued.queued, 1);

        drop(first);
        let second_permit = second.await.unwrap();
        let after_second = admission.stats("http://w1:8000");
        assert_eq!(after_second.inflight, 1);
        assert_eq!(after_second.queued, 0);

        drop(second_permit);
        assert_eq!(admission.stats("http://w1:8000"), expected_idle);
    }

    #[tokio::test]
    async fn limits_are_independent_per_worker() {
        let admission = WorkerAdmission::new(config(Some(1), 1, 1000));
        let a = admission
            .acquire("http://w1:8000")
            .await
            .unwrap()
            .expect("worker a");
        let b = admission
            .acquire("http://w2:8000")
            .await
            .unwrap()
            .expect("worker b should be independent");
        drop(a);
        drop(b);
    }
}
