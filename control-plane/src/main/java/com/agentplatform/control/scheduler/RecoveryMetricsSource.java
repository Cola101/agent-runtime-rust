package com.agentplatform.control.scheduler;

import java.time.Duration;

public interface RecoveryMetricsSource {
  RecoverySloSnapshot globalRecoverySloSnapshot(Duration objective);
}
