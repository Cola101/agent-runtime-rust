package com.agentplatform.control.scheduler;

@FunctionalInterface
public interface DispatchReconciler {
  ReconcileResult reconcile();
}
