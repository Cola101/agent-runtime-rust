package com.agentplatform.control.scheduler;

public final class DatabaseDispatchReconciler implements DispatchReconciler {
  private final JdbcSchedulerRepository repository;
  private final java.time.Duration leaseDuration;
  private final java.time.Duration heartbeatFreshness;

  public DatabaseDispatchReconciler(
      JdbcSchedulerRepository repository,
      java.time.Duration leaseDuration,
      java.time.Duration heartbeatFreshness) {
    this.repository = repository;
    this.leaseDuration = leaseDuration;
    this.heartbeatFreshness = heartbeatFreshness;
  }

  @Override
  public ReconcileResult reconcile() {
    return repository.reconcileExpired(leaseDuration, heartbeatFreshness);
  }
}
