package com.agentplatform.control.testing;

import com.agentplatform.control.messaging.NatsConnectionSettings;
import io.nats.client.Connection;
import io.nats.client.Nats;
import java.io.IOException;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.DriverManager;
import java.sql.SQLException;
import java.time.Duration;
import java.util.Locale;
import java.util.UUID;
import org.flywaydb.core.Flyway;
import org.springframework.jdbc.datasource.DriverManagerDataSource;

public final class NativeIntegrationEnvironment {
  private static final String JDBC_URL = requiredEnvironment("SPRING_DATASOURCE_URL");
  private static final String JDBC_USERNAME = requiredEnvironment("SPRING_DATASOURCE_USERNAME");
  private static final String JDBC_PASSWORD = requiredEnvironment("SPRING_DATASOURCE_PASSWORD");

  private NativeIntegrationEnvironment() {}

  public static NativeDatabase createDatabase(String owner) {
    var databaseName = databaseName(owner);
    var endpoints = databaseEndpoints(databaseName);
    executeAdmin(endpoints.adminUrl(), "create database \"" + databaseName + "\"");
    return new NativeDatabase(
        databaseName, endpoints.adminUrl(), endpoints.databaseUrl(), JDBC_USERNAME, JDBC_PASSWORD);
  }

  public static String natsUrl() {
    return requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_URL");
  }

  public static NatsConnectionSettings natsSettings() {
    return NatsConnectionSettings.secure(
        natsUrl(),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_CONTROL_USERNAME"),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_CONTROL_PASSWORD"),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE"),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD"));
  }

  public static Connection connectNats() throws IOException, InterruptedException {
    return Nats.connect(natsSettings().toOptions());
  }

  public static NatsConnectionSettings workerNatsSettings() {
    return NatsConnectionSettings.secure(
        natsUrl(),
        requiredEnvironment("AGENT_RUNTIME_NATS_USERNAME"),
        requiredEnvironment("AGENT_RUNTIME_NATS_PASSWORD"),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE"),
        requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD"));
  }

  public static Connection connectWorkerNats() throws IOException, InterruptedException {
    return Nats.connect(workerNatsSettings().toOptions());
  }

  public static String[] natsSecurityProperties(String prefix) {
    return new String[] {
      prefix + ".nats-url=" + natsUrl(),
      prefix + ".nats-security.tls-required=true",
      prefix + ".nats-security.username="
          + requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_CONTROL_USERNAME"),
      prefix + ".nats-security.password="
          + requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_CONTROL_PASSWORD"),
      prefix + ".nats-security.truststore-path="
          + requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE"),
      prefix + ".nats-security.truststore-password="
          + requiredEnvironment("AGENT_RUNTIME_LOCAL_NATS_TRUSTSTORE_PASSWORD")
    };
  }

  public static void pauseNats() {
    signalNats("-STOP");
  }

  public static void resumeNats() {
    signalNats("-CONT");
    awaitNatsReady();
  }

  private static void awaitNatsReady() {
    var deadline = System.nanoTime() + Duration.ofSeconds(10).toNanos();
    IOException lastFailure = null;
    while (System.nanoTime() < deadline) {
      try (var ignored = connectNats()) {
        return;
      } catch (IOException unavailable) {
        lastFailure = unavailable;
      } catch (InterruptedException interrupted) {
        Thread.currentThread().interrupt();
        throw new IllegalStateException("interrupted while waiting for resumed NATS", interrupted);
      }
      try {
        Thread.sleep(50);
      } catch (InterruptedException interrupted) {
        Thread.currentThread().interrupt();
        throw new IllegalStateException("interrupted while waiting for resumed NATS", interrupted);
      }
    }
    throw new IllegalStateException("resumed NATS did not become ready", lastFailure);
  }

  private static String requiredEnvironment(String name) {
    var value = System.getenv(name);
    if (value == null || value.isBlank()) {
      throw new IllegalStateException(
          name + " is required; run Java tests through deploy/native/run-java-tests");
    }
    return value;
  }

  private static String databaseName(String owner) {
    var prefix = owner.toLowerCase(Locale.ROOT).replaceAll("[^a-z0-9]", "_");
    if (prefix.length() > 24) {
      prefix = prefix.substring(0, 24);
    }
    return "test_" + prefix + "_" + UUID.randomUUID().toString().replace("-", "");
  }

  private static DatabaseEndpoints databaseEndpoints(String databaseName) {
    if (!JDBC_URL.startsWith("jdbc:")) {
      throw new IllegalStateException("unsupported JDBC URL: " + JDBC_URL);
    }
    var endpoint = URI.create(JDBC_URL.substring("jdbc:".length()));
    if (!"postgresql".equals(endpoint.getScheme()) || endpoint.getHost() == null) {
      throw new IllegalStateException("expected a PostgreSQL TCP JDBC URL");
    }
    var authority = endpoint.getHost() + (endpoint.getPort() < 0 ? "" : ":" + endpoint.getPort());
    var query = endpoint.getRawQuery() == null ? "" : "?" + endpoint.getRawQuery();
    return new DatabaseEndpoints(
        "jdbc:postgresql://" + authority + "/postgres" + query,
        "jdbc:postgresql://" + authority + "/" + databaseName + query);
  }

  private static void executeAdmin(String adminUrl, String sql) {
    try (var connection = DriverManager.getConnection(adminUrl, JDBC_USERNAME, JDBC_PASSWORD);
        var statement = connection.createStatement()) {
      statement.execute(sql);
    } catch (SQLException exception) {
      throw new IllegalStateException("failed to manage native integration database", exception);
    }
  }

  private static void signalNats(String signal) {
    var pidPath = Path.of(requiredEnvironment("AGENT_RUNTIME_NATS_PID_FILE"));
    try {
      var pid = Long.parseLong(Files.readString(pidPath).trim());
      var process = ProcessHandle.of(pid)
          .filter(ProcessHandle::isAlive)
          .orElseThrow(() -> new IllegalStateException("native NATS process is not running"));
      var command = process.info().command().orElse("");
      if (!Path.of(command).getFileName().toString().equals("nats-server")) {
        throw new IllegalStateException("refusing to signal unexpected process " + pid);
      }
      var result = new ProcessBuilder("/bin/kill", signal, Long.toString(pid)).start().waitFor();
      if (result != 0) {
        throw new IllegalStateException("failed to send " + signal + " to native NATS");
      }
    } catch (InterruptedException exception) {
      Thread.currentThread().interrupt();
      throw new IllegalStateException("interrupted while signalling native NATS", exception);
    } catch (Exception exception) {
      if (exception instanceof IllegalStateException illegalState) {
        throw illegalState;
      }
      throw new IllegalStateException("failed to signal native NATS", exception);
    }
  }

  private record DatabaseEndpoints(String adminUrl, String databaseUrl) {}

  public record NativeDatabase(
      String name, String adminUrl, String jdbcUrl, String username, String password)
      implements AutoCloseable {
    public void migrate() {
      Flyway.configure().dataSource(jdbcUrl, username, password).load().migrate();
    }

    public DriverManagerDataSource dataSource() {
      return new DriverManagerDataSource(jdbcUrl, username, password);
    }

    @Override
    public void close() {
      executeAdmin(adminUrl, "drop database if exists \"" + name + "\" with (force)");
    }
  }
}
