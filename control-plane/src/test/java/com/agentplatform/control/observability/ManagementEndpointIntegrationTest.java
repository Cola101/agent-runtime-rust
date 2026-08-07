package com.agentplatform.control.observability;

import static org.assertj.core.api.Assertions.assertThat;

import com.agentplatform.control.testing.NativeIntegrationEnvironment;
import com.agentplatform.control.testing.NativeIntegrationEnvironment.NativeDatabase;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.util.Base64;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.Test;
import org.springframework.boot.test.autoconfigure.actuate.observability.AutoConfigureObservability;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.boot.test.web.server.LocalManagementPort;
import org.springframework.boot.test.web.server.LocalServerPort;
import org.springframework.test.context.DynamicPropertyRegistry;
import org.springframework.test.context.DynamicPropertySource;

@SpringBootTest(webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT)
@AutoConfigureObservability
class ManagementEndpointIntegrationTest {
  private static final NativeDatabase DATABASE =
      NativeIntegrationEnvironment.createDatabase("management-endpoint");

  static {
    DATABASE.migrate();
  }

  @LocalServerPort
  private int apiPort;

  @LocalManagementPort
  private int managementPort;

  @DynamicPropertySource
  static void configureApplication(DynamicPropertyRegistry registry) {
    registry.add("spring.datasource.url", DATABASE::jdbcUrl);
    registry.add("spring.datasource.username", DATABASE::username);
    registry.add("spring.datasource.password", DATABASE::password);
    registry.add("spring.security.oauth2.resourceserver.jwt.jwk-set-uri",
        () -> "http://127.0.0.1:1/jwks");
    registry.add("spring.security.user.name", () -> "metrics-scraper");
    registry.add("spring.security.user.password", () -> "test-scrape-password");
  }

  @AfterAll
  static void stopDatabase() {
    DATABASE.close();
  }

  @Test
  void prometheusUsesDedicatedBasicAuthOnAnIndependentManagementPort() throws Exception {
    assertThat(managementPort).isNotEqualTo(apiPort);

    var client = HttpClient.newHttpClient();
    var health = client.send(
        HttpRequest.newBuilder()
            .uri(URI.create("http://127.0.0.1:" + managementPort
                + "/actuator/health/liveness"))
            .GET()
            .build(),
        HttpResponse.BodyHandlers.ofString());
    var endpoint = URI.create(
        "http://127.0.0.1:" + managementPort + "/actuator/prometheus");
    var anonymous = client.send(
        HttpRequest.newBuilder()
            .uri(endpoint)
            .GET()
            .build(),
        HttpResponse.BodyHandlers.ofString());
    var credentials = Base64.getEncoder().encodeToString(
        "metrics-scraper:test-scrape-password".getBytes(StandardCharsets.UTF_8));
    var authenticated = client.send(
        HttpRequest.newBuilder()
            .uri(endpoint)
            .header("Authorization", "Basic " + credentials)
            .GET()
            .build(),
        HttpResponse.BodyHandlers.ofString());

    assertThat(health.statusCode()).isEqualTo(200);
    assertThat(anonymous.statusCode()).isEqualTo(401);
    assertThat(anonymous.headers().firstValue("WWW-Authenticate"))
        .hasValueSatisfying(value -> assertThat(value).containsIgnoringCase("Basic"));
    assertThat(authenticated.statusCode()).isEqualTo(200);
    assertThat(authenticated.body()).contains("jvm_info");
    assertThat(authenticated.body()).doesNotContain("tenant_id");
  }
}
