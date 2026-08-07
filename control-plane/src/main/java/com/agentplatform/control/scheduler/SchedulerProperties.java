package com.agentplatform.control.scheduler;

import com.agentplatform.control.messaging.NatsSecurityProperties;
import java.time.Duration;
import org.springframework.boot.context.properties.ConfigurationProperties;

@ConfigurationProperties("agent.runtime.scheduler")
public class SchedulerProperties {
  private boolean enabled;
  private String natsUrl = "nats://127.0.0.1:4222";
  private String durableName = "runtime-scheduler-v1";
  private Duration leaseDuration = Duration.ofSeconds(30);
  private Duration heartbeatFreshness = Duration.ofSeconds(15);
  private Duration retryDelay = Duration.ofSeconds(1);
  private Duration pollTimeout = Duration.ofMillis(250);
  private Duration recoveryObjective = Duration.ofMinutes(15);
  private final NatsSecurityProperties natsSecurity = new NatsSecurityProperties();

  public boolean isEnabled() {
    return enabled;
  }

  public void setEnabled(boolean enabled) {
    this.enabled = enabled;
  }

  public String getNatsUrl() {
    return natsUrl;
  }

  public void setNatsUrl(String natsUrl) {
    this.natsUrl = natsUrl;
  }

  public String getDurableName() {
    return durableName;
  }

  public void setDurableName(String durableName) {
    this.durableName = durableName;
  }

  public Duration getLeaseDuration() {
    return leaseDuration;
  }

  public void setLeaseDuration(Duration leaseDuration) {
    this.leaseDuration = leaseDuration;
  }

  public Duration getHeartbeatFreshness() {
    return heartbeatFreshness;
  }

  public void setHeartbeatFreshness(Duration heartbeatFreshness) {
    this.heartbeatFreshness = heartbeatFreshness;
  }

  public Duration getRetryDelay() {
    return retryDelay;
  }

  public void setRetryDelay(Duration retryDelay) {
    this.retryDelay = retryDelay;
  }

  public Duration getPollTimeout() {
    return pollTimeout;
  }

  public Duration getRecoveryObjective() {
    return recoveryObjective;
  }

  public void setRecoveryObjective(Duration recoveryObjective) {
    this.recoveryObjective = recoveryObjective;
  }

  public void setPollTimeout(Duration pollTimeout) {
    this.pollTimeout = pollTimeout;
  }

  public NatsSecurityProperties getNatsSecurity() {
    return natsSecurity;
  }
}
