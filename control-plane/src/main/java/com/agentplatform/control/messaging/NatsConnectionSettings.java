package com.agentplatform.control.messaging;

import io.nats.client.Options;
import java.security.NoSuchAlgorithmException;
import java.util.Objects;

/** Validated NATS connection settings with secret-safe diagnostics. */
public final class NatsConnectionSettings {
  private final String server;
  private final String username;
  private final String password;
  private final String truststorePath;
  private final String truststorePassword;
  private final boolean tlsRequired;

  private NatsConnectionSettings(
      String server,
      String username,
      String password,
      String truststorePath,
      String truststorePassword,
      boolean tlsRequired) {
    this.server = required("server", server);
    this.username = username;
    this.password = password;
    this.truststorePath = truststorePath;
    this.truststorePassword = truststorePassword;
    this.tlsRequired = tlsRequired;
  }

  public static NatsConnectionSettings insecureForDevelopment(String server) {
    return new NatsConnectionSettings(server, null, null, null, null, false);
  }

  public static NatsConnectionSettings secure(
      String server,
      String username,
      String password,
      String truststorePath,
      String truststorePassword) {
    if (!required("server", server).startsWith("tls://")) {
      throw new IllegalArgumentException("secure NATS server must use tls://");
    }
    return new NatsConnectionSettings(
        server,
        required("username", username),
        required("password", password),
        required("truststorePath", truststorePath),
        required("truststorePassword", truststorePassword),
        true);
  }

  public Options toOptions() {
    var builder = new Options.Builder().server(server);
    if (!tlsRequired) {
      return builder.build();
    }
    try {
      return builder
          .secure()
          .userInfo(username, password)
          .truststorePath(truststorePath)
          .truststorePassword(truststorePassword.toCharArray())
          .build();
    } catch (NoSuchAlgorithmException exception) {
      throw new IllegalStateException("TLS is unavailable for the NATS client", exception);
    }
  }

  private static String required(String field, String value) {
    Objects.requireNonNull(value, field + " must not be null");
    if (value.isBlank()) {
      throw new IllegalArgumentException(field + " must not be blank");
    }
    return value;
  }

  @Override
  public String toString() {
    return "NatsConnectionSettings[server="
        + server
        + ", username="
        + (username == null ? "none" : username)
        + ", tlsRequired="
        + tlsRequired
        + "]";
  }
}
