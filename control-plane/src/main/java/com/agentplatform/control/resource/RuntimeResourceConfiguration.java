package com.agentplatform.control.resource;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.security.KeyFactory;
import java.security.spec.PKCS8EncodedKeySpec;
import java.util.Base64;
import org.springframework.beans.factory.annotation.Value;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

@Configuration
class RuntimeResourceConfiguration {
  @Bean
  ProviderCredentialSealer providerCredentialSealer(
      @Value("${agent.runtime.model-gateway.credential-public-key-path:}") String publicKeyPath,
      ObjectMapper mapper) {
    return new RsaAesGcmProviderCredentialSealer(publicKeyPath, mapper);
  }

  @Bean
  RuntimeResourceService runtimeResourceService(
      RuntimeResourceRepository repository,
      ProviderCredentialSealer credentialSealer,
      SkillArtifactSigner skillArtifactSigner) {
    return new RuntimeResourceService(repository, credentialSealer, skillArtifactSigner);
  }

  @Bean
  SkillArtifactSigner skillArtifactSigner(
      @Value("${agent.runtime.skill-signing.private-key-pkcs8:}") String encodedPrivateKey,
      @Value("${agent.runtime.skill-signing.key-id:}") String keyId,
      ObjectMapper mapper) {
    if (encodedPrivateKey.isBlank() || keyId.isBlank()) {
      return artifact -> {
        throw new IllegalStateException("skill artifact signing is not configured");
      };
    }
    try {
      var privateKey = KeyFactory.getInstance("Ed25519").generatePrivate(
          new PKCS8EncodedKeySpec(Base64.getDecoder().decode(encodedPrivateKey)));
      return new Ed25519SkillArtifactSigner(keyId, privateKey, mapper);
    } catch (IllegalArgumentException | java.security.GeneralSecurityException exception) {
      throw new IllegalStateException("invalid Ed25519 skill signing private key", exception);
    }
  }
}
