package com.agentplatform.control.messaging;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import java.io.OutputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

class NatsConnectionSettingsTest {
  @Test
  void secureSettingsRequireTlsCredentialsAndTruststore() {
    assertThatThrownBy(
            () ->
                NatsConnectionSettings.secure(
                    "nats://nats.agent-runtime.svc:4222",
                    "control-plane",
                    "secret",
                    "/var/run/secrets/nats/truststore.p12",
                    "changeit"))
        .isInstanceOf(IllegalArgumentException.class)
        .hasMessageContaining("tls://");

    assertThatThrownBy(
            () ->
                NatsConnectionSettings.secure(
                    "tls://nats.agent-runtime.svc:4222",
                    "control-plane",
                    " ",
                    "/var/run/secrets/nats/truststore.p12",
                    "changeit"))
        .isInstanceOf(IllegalArgumentException.class)
        .hasMessageContaining("password");
  }

  @Test
  void secureOptionsEnableTlsAndUserAuthenticationWithoutLeakingPassword(@TempDir Path directory)
      throws Exception {
    var truststore = directory.resolve("truststore.p12");
    var keyStore = KeyStore.getInstance("PKCS12");
    keyStore.load(null, null);
    try (OutputStream output = Files.newOutputStream(truststore)) {
      keyStore.store(output, "truststore-password".toCharArray());
    }
    var settings =
        NatsConnectionSettings.secure(
            "tls://nats.agent-runtime.svc:4222",
            "control-plane",
            "very-sensitive-password",
            truststore.toString(),
            "truststore-password");

    var options = settings.toOptions();

    assertThat(options.getServers())
        .extracting(Object::toString)
        .containsExactly("tls://nats.agent-runtime.svc:4222");
    assertThat(options.getUsername()).isEqualTo("control-plane");
    assertThat(options.getPassword()).isEqualTo("very-sensitive-password");
    assertThat(options.getSslContext()).isNotNull();
    assertThat(settings.toString()).doesNotContain("very-sensitive-password", "truststore-password");
  }
}
