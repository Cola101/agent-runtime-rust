package com.agentplatform.control.identity;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.security.KeyFactory;
import java.security.spec.PKCS8EncodedKeySpec;
import java.time.Clock;
import java.util.Base64;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;

@Configuration(proxyBeanMethods = false)
@ConditionalOnProperty(prefix = "agent.runtime.scheduler", name = "enabled", havingValue = "true")
public class WorkloadIdentityConfiguration {
  @Bean
  WorkloadTokenIssuer workloadTokenIssuer(
      @Value("${agent.runtime.workload-identity.private-key-pkcs8}") String encodedPrivateKey,
      ObjectMapper objectMapper) {
    try {
      var privateKey = KeyFactory.getInstance("Ed25519").generatePrivate(
          new PKCS8EncodedKeySpec(Base64.getDecoder().decode(encodedPrivateKey)));
      return new Ed25519WorkloadTokenIssuer(privateKey, objectMapper, Clock.systemUTC());
    } catch (IllegalArgumentException | java.security.GeneralSecurityException exception) {
      throw new IllegalStateException("invalid Ed25519 workload identity private key", exception);
    }
  }
}
