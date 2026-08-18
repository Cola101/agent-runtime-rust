//! The Runtime as its owner sees it: something that starts, reports itself,
//! and stops.
//!
//! Deliberately not part of `RuntimeClient`. A workload asks the Runtime to do
//! work; whoever owns the state root asks it to exist or to stop existing, and
//! a tenant that could do the second one could take the Runtime away from
//! everybody else. The two are different authorities and they get different
//! types.
//!
//! The lifecycle is `Created → Recovering → Ready → Draining → Stopped`, in
//! that order and once. A stopped instance is not restarted: the state root
//! lease, the recovered Runs and the owner epochs all belong to one pass
//! through this machine, and pretending otherwise would hand a second pass the
//! first one's assumptions.

use crate::client::RuntimeClientError;
use crate::embedded::EmbeddedRuntime;
use agent_protocol::RuntimeInvocationContext;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// How long `shutdown` waits for work already admitted, before it stops the
/// Runtime out from under it.
///
/// A constructor argument rather than a constant, for the reason the Session
/// ceilings are: a test that has to sit through the real value cannot check the
/// boundary, and a bound nothing exercises is a bound nobody has verified. The
/// published range is what an owner may choose; the default is what they get.
pub const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(10);
pub const MAX_DRAIN_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLifecycle {
    Created,
    Recovering,
    Ready,
    Draining,
    Stopped,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeControllerError {
    /// The instance is past the point where this could be done. Distinct from a
    /// refusal a caller can retry: nothing about this instance will change back.
    #[error("Runtime is {0:?} and cannot start")]
    NotStartable(RuntimeLifecycle),
}

/// What one pass through the lifecycle left behind.
///
/// Returned to whoever called `shutdown`, and — because for a desktop
/// application that caller is a process on its way out and will not read it —
/// also carried to the next `start` so a person can be told what happened last
/// time.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RuntimeShutdownReport {
    pub active_before_drain: usize,
    pub queued_before_drain: usize,
    /// Woken with `Unavailable` when admission closed, rather than left waiting
    /// on a Runtime that is leaving.
    pub released_from_queue: usize,
    pub completed_during_drain: usize,
    /// Still running when the deadline arrived, and stopped by the Runtime.
    ///
    /// Stopped, not cancelled: no `run.cancelled` is published and no operator
    /// Cancel receipt is written, because nobody decided to cancel them.
    pub stopped_at_deadline: usize,
    /// Stopped with a verifiable Checkpoint, so a replacement picks them up.
    pub left_for_recovery: usize,
    /// Stopped before producing one, and recorded as interrupted by a Runtime
    /// that was stopped -- not by one that died, and not by anybody cancelling.
    pub interrupted: usize,
    pub deadline_reached: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RuntimeRecoveryProgress {
    pub completed_profiles: usize,
    pub total_profiles: usize,
}

/// One Profile whose startup reconciliation could not be completed.
///
/// The failure is carried as a stable client error rather than the raw
/// embedded one. An owner owns these paths, but a message that happens to
/// contain one still ends up in logs and screenshots, and the published
/// vocabulary already says everything a caller can act on.
#[derive(Clone, Debug)]
pub struct RuntimeProfileRecoveryFailure {
    pub invocation: RuntimeInvocationContext,
    pub error: RuntimeClientError,
}

/// What an owner can see without being able to change anything.
#[derive(Clone, Debug)]
pub struct RuntimeControllerSnapshot {
    pub lifecycle: RuntimeLifecycle,
    pub recovery: RuntimeRecoveryProgress,
    pub active_runs: usize,
    pub queued_runs: usize,
    pub recovery_failures: Vec<RuntimeProfileRecoveryFailure>,
    /// Present until it has been handed over once.
    pub previous_shutdown: Option<RuntimeShutdownReport>,
}

struct ControllerState {
    lifecycle: RuntimeLifecycle,
    recovery: RuntimeRecoveryProgress,
    recovery_failures: Vec<RuntimeProfileRecoveryFailure>,
    report: Option<RuntimeShutdownReport>,
    previous_shutdown: Option<RuntimeShutdownReport>,
}

pub struct RuntimeController {
    runtime: Arc<EmbeddedRuntime>,
    state: Mutex<ControllerState>,
    /// Woken on every lifecycle transition. `start` and `shutdown` may each be
    /// called by several callers at once, and all of them must be told the same
    /// thing by the one pass that actually ran.
    changed: Notify,
    drain_deadline: Duration,
}

impl RuntimeController {
    #[must_use]
    pub fn new(runtime: Arc<EmbeddedRuntime>) -> Arc<Self> {
        Self::with_drain_deadline(runtime, DEFAULT_DRAIN_DEADLINE)
    }

    #[must_use]
    pub fn with_drain_deadline(
        runtime: Arc<EmbeddedRuntime>,
        drain_deadline: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            state: Mutex::new(ControllerState {
                lifecycle: RuntimeLifecycle::Created,
                recovery: RuntimeRecoveryProgress::default(),
                recovery_failures: Vec::new(),
                report: None,
                previous_shutdown: None,
            }),
            changed: Notify::new(),
            drain_deadline: drain_deadline.min(MAX_DRAIN_DEADLINE),
        })
    }

    pub async fn lifecycle(&self) -> RuntimeLifecycle {
        self.state.lock().await.lifecycle
    }

    pub async fn snapshot(&self) -> RuntimeControllerSnapshot {
        let runtime = self.runtime.runtime_snapshot();
        let mut state = self.state.lock().await;
        RuntimeControllerSnapshot {
            lifecycle: state.lifecycle,
            recovery: state.recovery.clone(),
            active_runs: runtime.active_execution_owners,
            queued_runs: runtime.admission.queued_runs,
            recovery_failures: state.recovery_failures.clone(),
            // Handed over once. Leaving it in place would make every later
            // snapshot report a shutdown that has already been accounted for.
            previous_shutdown: state.previous_shutdown.take(),
        }
    }

    /// Recovers every Profile, then opens for work.
    ///
    /// Concurrent callers do not each run recovery: the first claims the
    /// transition and the rest wait for it, and all of them get the same
    /// answer. Recovery failures are per-Profile and do not stop the rest --
    /// one tenant's unreadable state is not a reason to refuse another's work.
    pub async fn start(self: &Arc<Self>) -> Result<(), RuntimeControllerError> {
        {
            let mut state = self.state.lock().await;
            match state.lifecycle {
                RuntimeLifecycle::Created => {
                    state.lifecycle = RuntimeLifecycle::Recovering;
                    state.recovery = RuntimeRecoveryProgress {
                        completed_profiles: 0,
                        total_profiles: self.runtime.runtime_snapshot().registered_profiles,
                    };
                }
                RuntimeLifecycle::Ready => return Ok(()),
                RuntimeLifecycle::Recovering => {
                    drop(state);
                    return self.await_started().await;
                }
                other => return Err(RuntimeControllerError::NotStartable(other)),
            }
        }
        self.changed.notify_waiters();

        let report = self.runtime.recover_all_unfinished_detached().await;

        let mut state = self.state.lock().await;
        state.recovery.completed_profiles = state.recovery.total_profiles;
        state.recovery_failures = report
            .failures
            .into_iter()
            .map(|failure| RuntimeProfileRecoveryFailure {
                invocation: failure.invocation,
                error: RuntimeClientError::from_embedded(failure.error),
            })
            .collect();
        state.lifecycle = RuntimeLifecycle::Ready;
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Stops taking work, waits a bounded time for what was already admitted,
    /// and reports what it found.
    ///
    /// Concurrent callers do not each drain: the first claims the transition
    /// and the rest are handed the same report, because "how did the shutdown
    /// go" has one answer per instance and two callers must not be told
    /// different ones.
    ///
    /// The wait is on the active-execution count, not on a duration. It
    /// converges the moment the last admitted Run finishes; the deadline only
    /// bounds how long an owner is made to wait for work that will not.
    pub async fn shutdown(self: &Arc<Self>) -> RuntimeShutdownReport {
        let before = {
            let mut state = self.state.lock().await;
            match state.lifecycle {
                RuntimeLifecycle::Stopped => {
                    return state.report.clone().unwrap_or_default();
                }
                RuntimeLifecycle::Draining => {
                    drop(state);
                    return self.await_stopped().await;
                }
                _ => {
                    state.lifecycle = RuntimeLifecycle::Draining;
                    self.runtime.runtime_snapshot()
                }
            }
        };
        self.changed.notify_waiters();

        // Nothing new is admitted from here, and everyone still queued is told
        // so rather than left to wait it out. Runs that already hold a permit
        // keep it: they are executing, and taking the slot back would not stop
        // them, only lose count of them.
        let released_from_queue = self.runtime.close_admission();

        let active_before = before.active_execution_owners;
        let queued_before = before.admission.queued_runs;
        let deadline_reached = tokio::time::timeout(self.drain_deadline, async {
            while self.runtime.runtime_snapshot().active_execution_owners > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err();

        // Whatever is still detached at this point is stopped, not cancelled.
        // The two are different events and travel different paths: this one
        // never touches a `CancellationToken`, so nothing here decides that a
        // person asked for the Run to end.
        // Taken before the abort: aborting drops each task's guard, so the
        // active map empties behind us and there would be nothing left to
        // account for.
        let stopped_keys = self.runtime.active_execution_keys();
        let stopped_at_deadline = self.runtime.stop_background_executions();
        let (left_for_recovery, interrupted) = self.runtime.account_for_stopped_runs(&stopped_keys);
        let remaining = self.runtime.runtime_snapshot().active_execution_owners;
        let report = RuntimeShutdownReport {
            active_before_drain: active_before,
            queued_before_drain: queued_before,
            released_from_queue,
            completed_during_drain: active_before.saturating_sub(remaining),
            stopped_at_deadline,
            left_for_recovery,
            interrupted,
            deadline_reached,
        };

        let mut state = self.state.lock().await;
        state.lifecycle = RuntimeLifecycle::Stopped;
        state.report = Some(report.clone());
        state.previous_shutdown = Some(report.clone());
        drop(state);
        self.changed.notify_waiters();
        report
    }

    async fn await_stopped(self: &Arc<Self>) -> RuntimeShutdownReport {
        loop {
            let notified = self.changed.notified();
            {
                let state = self.state.lock().await;
                if state.lifecycle == RuntimeLifecycle::Stopped {
                    return state.report.clone().unwrap_or_default();
                }
            }
            notified.await;
        }
    }

    async fn await_started(self: &Arc<Self>) -> Result<(), RuntimeControllerError> {
        loop {
            let notified = self.changed.notified();
            match self.state.lock().await.lifecycle {
                RuntimeLifecycle::Ready => return Ok(()),
                RuntimeLifecycle::Recovering => {}
                other => return Err(RuntimeControllerError::NotStartable(other)),
            }
            notified.await;
        }
    }
}
