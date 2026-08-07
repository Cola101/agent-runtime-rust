package com.agentplatform.control.scheduler;

import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.scheduling.annotation.Scheduled;

public final class ScheduledDispatchReconciler {
  private static final Logger LOGGER = LoggerFactory.getLogger(ScheduledDispatchReconciler.class);
  private final DispatchReconciler reconciler;

  public ScheduledDispatchReconciler(DispatchReconciler reconciler) {
    this.reconciler = reconciler;
  }

  @Scheduled(fixedDelayString = "${agent.runtime.scheduler.reconcile-delay-ms:1000}")
  public ReconcileResult reconcile() {
    var result = reconciler.reconcile();
    if (result.requeued() > 0 || result.recovered() > 0 || result.indeterminate() > 0
        || result.failed() > 0) {
      LOGGER.warn(
          "Reconciled expired assignments: requeued={}, recovered={}, indeterminate={}, failed={}",
          result.requeued(), result.recovered(), result.indeterminate(), result.failed());
    }
    return result;
  }
}
