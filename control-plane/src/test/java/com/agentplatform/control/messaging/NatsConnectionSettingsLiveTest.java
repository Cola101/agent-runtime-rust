package com.agentplatform.control.messaging;

import static org.assertj.core.api.Assertions.assertThatThrownBy;

import io.nats.client.Nats;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;

class NatsConnectionSettingsLiveTest {
  @Test
  @EnabledIfEnvironmentVariable(named = "TEST_NATS_URL", matches = "tls://.+")
  void authenticatesAgainstLiveTlsServerAndRejectsInvalidPassword() throws Exception {
    var environment = System.getenv();
    var valid =
        NatsConnectionSettings.secure(
            environment.get("TEST_NATS_URL"),
            environment.get("TEST_NATS_USERNAME"),
            environment.get("TEST_NATS_PASSWORD"),
            environment.get("TEST_NATS_TRUSTSTORE"),
            environment.get("TEST_NATS_TRUSTSTORE_PASSWORD"));
    try (var connection = Nats.connect(valid.toOptions())) {
      connection.flush(java.time.Duration.ofSeconds(2));
    }

    var invalid =
        NatsConnectionSettings.secure(
            environment.get("TEST_NATS_URL"),
            environment.get("TEST_NATS_USERNAME"),
            "wrong-password",
            environment.get("TEST_NATS_TRUSTSTORE"),
            environment.get("TEST_NATS_TRUSTSTORE_PASSWORD"));
    assertThatThrownBy(() -> Nats.connect(invalid.toOptions())).isInstanceOf(Exception.class);
  }
}
