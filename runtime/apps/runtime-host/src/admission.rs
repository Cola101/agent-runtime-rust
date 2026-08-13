//! Bounded, tenant-fair Run admission for embedded and standalone runtimes.
//!
//! This layer schedules complete Runs. Provider routing and MCP discovery have
//! their own narrower limits and cannot protect the Runtime itself from one
//! tenant consuming every execution slot.

use agent_protocol::RuntimeInvocationContext;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeAdmissionLimits {
    pub max_active_runs: usize,
    pub max_active_runs_per_tenant: usize,
    pub max_active_runs_per_workspace: usize,
    pub max_queued_runs: usize,
    pub max_queued_runs_per_tenant: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeAdmissionSnapshot {
    pub active_runs: usize,
    pub queued_runs: usize,
    pub active_tenants: usize,
    pub active_workspaces: usize,
    pub queued_tenants: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RuntimeAdmissionError {
    #[error("Runtime admission limits are invalid")]
    InvalidLimits,
    #[error("Runtime invocation is invalid: {0}")]
    InvalidInvocation(String),
    #[error("tenant queue is full")]
    TenantQueueFull,
    #[error("global queue is full")]
    GlobalQueueFull,
    #[error("Runtime admission stopped before granting a slot")]
    Closed,
}

struct WaitingRun {
    request_id: Uuid,
    invocation: RuntimeInvocationContext,
    grant: oneshot::Sender<RuntimeAdmissionPermit>,
}

struct QueuedRequestGuard {
    controller: Weak<RuntimeAdmissionController>,
    tenant_id: Uuid,
    request_id: Uuid,
    armed: bool,
}

impl Drop for QueuedRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(controller) = self.controller.upgrade() {
            controller.cancel_waiter(self.tenant_id, self.request_id);
        }
    }
}

#[derive(Default)]
struct AdmissionState {
    active_runs: usize,
    queued_runs: usize,
    active_by_tenant: HashMap<Uuid, usize>,
    active_by_workspace: HashMap<(Uuid, Uuid, Uuid), usize>,
    queues: HashMap<Uuid, VecDeque<WaitingRun>>,
    rotation: VecDeque<Uuid>,
    last_admitted_tenant: Option<Uuid>,
}

/// A process-local coordinator suitable for an embedded Runtime or a local
/// node. Distributed placement remains a control-plane concern; this object is
/// the final backpressure boundary before expensive Host construction.
pub struct RuntimeAdmissionController {
    limits: RuntimeAdmissionLimits,
    state: Mutex<AdmissionState>,
}

impl RuntimeAdmissionController {
    pub fn new(limits: RuntimeAdmissionLimits) -> Result<Self, RuntimeAdmissionError> {
        if limits.max_active_runs == 0
            || limits.max_active_runs_per_tenant == 0
            || limits.max_active_runs_per_tenant > limits.max_active_runs
            || limits.max_active_runs_per_workspace == 0
            || limits.max_active_runs_per_workspace > limits.max_active_runs
            || limits.max_queued_runs == 0
            || limits.max_queued_runs_per_tenant == 0
            || limits.max_queued_runs_per_tenant > limits.max_queued_runs
        {
            return Err(RuntimeAdmissionError::InvalidLimits);
        }
        Ok(Self {
            limits,
            state: Mutex::new(AdmissionState::default()),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> RuntimeAdmissionSnapshot {
        let state = self.lock_state();
        RuntimeAdmissionSnapshot {
            active_runs: state.active_runs,
            queued_runs: state.queued_runs,
            active_tenants: state.active_by_tenant.len(),
            active_workspaces: state.active_by_workspace.len(),
            queued_tenants: state.queues.len(),
        }
    }

    pub async fn acquire(
        self: &Arc<Self>,
        invocation: RuntimeInvocationContext,
    ) -> Result<RuntimeAdmissionPermit, RuntimeAdmissionError> {
        invocation
            .validate()
            .map_err(|error| RuntimeAdmissionError::InvalidInvocation(error.to_string()))?;
        let tenant_id = invocation.tenant_id;
        let (receiver, mut guard) = {
            let mut state = self.lock_state();
            let tenant_active = state.active_by_tenant.get(&tenant_id).copied().unwrap_or(0);
            let workspace_active = state
                .active_by_workspace
                .get(&Self::workspace_key(invocation))
                .copied()
                .unwrap_or(0);
            if state.queued_runs == 0
                && state.active_runs < self.limits.max_active_runs
                && tenant_active < self.limits.max_active_runs_per_tenant
                && workspace_active < self.limits.max_active_runs_per_workspace
            {
                Self::mark_active(&mut state, invocation);
                state.last_admitted_tenant = Some(tenant_id);
                return Ok(RuntimeAdmissionPermit::new(self, invocation));
            }

            let tenant_queued = state.queues.get(&tenant_id).map_or(0, VecDeque::len);
            if tenant_queued >= self.limits.max_queued_runs_per_tenant {
                return Err(RuntimeAdmissionError::TenantQueueFull);
            }
            if state.queued_runs >= self.limits.max_queued_runs {
                return Err(RuntimeAdmissionError::GlobalQueueFull);
            }

            let (grant, receiver) = oneshot::channel();
            let request_id = Uuid::now_v7();
            if tenant_queued == 0 {
                state.rotation.push_back(tenant_id);
            }
            state
                .queues
                .entry(tenant_id)
                .or_default()
                .push_back(WaitingRun {
                    request_id,
                    invocation,
                    grant,
                });
            state.queued_runs += 1;
            self.promote_locked(&mut state);
            (
                receiver,
                QueuedRequestGuard {
                    controller: Arc::downgrade(self),
                    tenant_id,
                    request_id,
                    armed: true,
                },
            )
        };
        let permit = receiver.await.map_err(|_| RuntimeAdmissionError::Closed)?;
        guard.armed = false;
        Ok(permit)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AdmissionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn workspace_key(invocation: RuntimeInvocationContext) -> (Uuid, Uuid, Uuid) {
        (
            invocation.tenant_id,
            invocation.application_id,
            invocation.workspace_id,
        )
    }

    fn mark_active(state: &mut AdmissionState, invocation: RuntimeInvocationContext) {
        state.active_runs += 1;
        *state
            .active_by_tenant
            .entry(invocation.tenant_id)
            .or_default() += 1;
        *state
            .active_by_workspace
            .entry(Self::workspace_key(invocation))
            .or_default() += 1;
    }

    fn unmark_active(state: &mut AdmissionState, invocation: RuntimeInvocationContext) {
        state.active_runs = state.active_runs.saturating_sub(1);
        if let Some(active) = state.active_by_tenant.get_mut(&invocation.tenant_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_by_tenant.remove(&invocation.tenant_id);
            }
        }
        let key = Self::workspace_key(invocation);
        if let Some(active) = state.active_by_workspace.get_mut(&key) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                state.active_by_workspace.remove(&key);
            }
        }
    }

    fn promote_locked(self: &Arc<Self>, state: &mut AdmissionState) {
        while state.active_runs < self.limits.max_active_runs {
            let eligible = state
                .rotation
                .iter()
                .copied()
                .filter(|tenant_id| {
                    state.queues.get(tenant_id).is_some_and(|queue| {
                        queue.iter().any(|waiting| {
                            state
                                .active_by_workspace
                                .get(&Self::workspace_key(waiting.invocation))
                                .copied()
                                .unwrap_or(0)
                                < self.limits.max_active_runs_per_workspace
                        })
                    }) && state.active_by_tenant.get(tenant_id).copied().unwrap_or(0)
                        < self.limits.max_active_runs_per_tenant
                })
                .collect::<Vec<_>>();
            let Some(tenant_id) = eligible
                .iter()
                .copied()
                .find(|tenant_id| Some(*tenant_id) != state.last_admitted_tenant)
                .or_else(|| eligible.first().copied())
            else {
                break;
            };

            if let Some(index) = state
                .rotation
                .iter()
                .position(|candidate| *candidate == tenant_id)
            {
                state.rotation.remove(index);
            }
            let waiting_index = state
                .queues
                .get(&tenant_id)
                .and_then(|queue| {
                    queue.iter().position(|waiting| {
                        state
                            .active_by_workspace
                            .get(&Self::workspace_key(waiting.invocation))
                            .copied()
                            .unwrap_or(0)
                            < self.limits.max_active_runs_per_workspace
                    })
                })
                .expect("eligible tenant has an eligible workspace");
            let waiting = state
                .queues
                .get_mut(&tenant_id)
                .and_then(|queue| queue.remove(waiting_index))
                .expect("eligible waiter remains queued");
            state.queued_runs = state.queued_runs.saturating_sub(1);
            if state
                .queues
                .get(&tenant_id)
                .is_some_and(|queue| !queue.is_empty())
            {
                state.rotation.push_back(tenant_id);
            } else {
                state.queues.remove(&tenant_id);
            }

            Self::mark_active(state, waiting.invocation);
            state.last_admitted_tenant = Some(tenant_id);
            let permit = RuntimeAdmissionPermit::new(self, waiting.invocation);
            if let Err(mut abandoned) = waiting.grant.send(permit) {
                abandoned.armed = false;
                Self::unmark_active(state, waiting.invocation);
            }
        }
    }

    fn release(self: &Arc<Self>, invocation: RuntimeInvocationContext) {
        let mut state = self.lock_state();
        Self::unmark_active(&mut state, invocation);
        self.promote_locked(&mut state);
    }

    fn cancel_waiter(self: &Arc<Self>, tenant_id: Uuid, request_id: Uuid) {
        let mut state = self.lock_state();
        let removed = state.queues.get_mut(&tenant_id).is_some_and(|queue| {
            let before = queue.len();
            queue.retain(|waiting| waiting.request_id != request_id);
            queue.len() != before
        });
        if !removed {
            return;
        }
        state.queued_runs = state.queued_runs.saturating_sub(1);
        if state.queues.get(&tenant_id).is_some_and(VecDeque::is_empty) {
            state.queues.remove(&tenant_id);
            state.rotation.retain(|queued| *queued != tenant_id);
        }
        self.promote_locked(&mut state);
    }
}

/// RAII capacity receipt. Cancellation and unwinding release a slot without a
/// separate cleanup call, which is essential for embedded callers.
pub struct RuntimeAdmissionPermit {
    controller: Weak<RuntimeAdmissionController>,
    invocation: RuntimeInvocationContext,
    armed: bool,
}

impl RuntimeAdmissionPermit {
    fn new(
        controller: &Arc<RuntimeAdmissionController>,
        invocation: RuntimeInvocationContext,
    ) -> Self {
        Self {
            controller: Arc::downgrade(controller),
            invocation,
            armed: true,
        }
    }
}

impl fmt::Debug for RuntimeAdmissionPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeAdmissionPermit")
            .field("tenant_id", &self.invocation.tenant_id)
            .field("workspace_id", &self.invocation.workspace_id)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl Drop for RuntimeAdmissionPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(controller) = self.controller.upgrade() {
            controller.release(self.invocation);
        }
    }
}
