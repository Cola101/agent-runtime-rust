package com.agentplatform.control.outbox;

import com.agentplatform.control.messaging.NatsSecurityProperties;
import java.time.Duration;
import org.springframework.boot.context.properties.ConfigurationProperties;

@ConfigurationProperties("agent.runtime.outbox")
public class OutboxProperties {
  private boolean enabled;
  private String natsUrl = "nats://127.0.0.1:4222";
  private int batchSize = 100;
  private Duration claimDuration = Duration.ofSeconds(30);
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

  public int getBatchSize() {
    return batchSize;
  }

  public void setBatchSize(int batchSize) {
    this.batchSize = batchSize;
  }

  public Duration getClaimDuration() {
    return claimDuration;
  }

  public void setClaimDuration(Duration claimDuration) {
    this.claimDuration = claimDuration;
  }

  public NatsSecurityProperties getNatsSecurity() {
    return natsSecurity;
  }
}
