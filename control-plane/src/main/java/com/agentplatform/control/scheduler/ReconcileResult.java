package com.agentplatform.control.scheduler;

/**
 * @param failed dispatches terminated because no Worker ever accepted the
 *     assignment. Distinct from {@code indeterminate}, which means a Worker did
 *     run and its side effects cannot be replayed safely.
 */
public record ReconcileResult(int requeued, int recovered, int indeterminate, int failed) {
  public ReconcileResult(int requeued, int recovered, int indeterminate) {
    this(requeued, recovered, indeterminate, 0);
  }

  public ReconcileResult(int requeued, int indeterminate) {
    this(requeued, 0, indeterminate, 0);
  }

  public ReconcileResult {
    if (requeued < 0 || recovered < 0 || indeterminate < 0 || failed < 0) {
      throw new IllegalArgumentException("reconcile counts must not be negative");
    }
  }
}
