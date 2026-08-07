package com.agentplatform.control.messaging;

/** Bindable NATS transport security properties shared by control-plane modules. */
public class NatsSecurityProperties {
  private boolean tlsRequired;
  private String username;
  private String password;
  private String truststorePath;
  private String truststorePassword;

  public NatsConnectionSettings settingsFor(String server) {
    if (!tlsRequired && !server.startsWith("tls://")) {
      return NatsConnectionSettings.insecureForDevelopment(server);
    }
    return NatsConnectionSettings.secure(
        server, username, password, truststorePath, truststorePassword);
  }

  public boolean isTlsRequired() {
    return tlsRequired;
  }

  public void setTlsRequired(boolean tlsRequired) {
    this.tlsRequired = tlsRequired;
  }

  public String getUsername() {
    return username;
  }

  public void setUsername(String username) {
    this.username = username;
  }

  public String getPassword() {
    return password;
  }

  public void setPassword(String password) {
    this.password = password;
  }

  public String getTruststorePath() {
    return truststorePath;
  }

  public void setTruststorePath(String truststorePath) {
    this.truststorePath = truststorePath;
  }

  public String getTruststorePassword() {
    return truststorePassword;
  }

  public void setTruststorePassword(String truststorePassword) {
    this.truststorePassword = truststorePassword;
  }
}
