package com.agentplatform.control.resource;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyFactory;
import java.security.MessageDigest;
import java.security.SecureRandom;
import java.security.interfaces.RSAPublicKey;
import java.security.spec.MGF1ParameterSpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.Arrays;
import java.util.Base64;
import java.util.HexFormat;
import java.util.Objects;
import java.util.UUID;
import javax.crypto.Cipher;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.OAEPParameterSpec;
import javax.crypto.spec.PSource;
import javax.crypto.spec.SecretKeySpec;

public final class RsaAesGcmProviderCredentialSealer implements ProviderCredentialSealer {
  private static final String ALGORITHM = "RSA-OAEP-256+A256GCM";
  private final String publicKeyPath;
  private final ObjectMapper mapper;
  private final SecureRandom random = new SecureRandom();

  public RsaAesGcmProviderCredentialSealer(String publicKeyPath, ObjectMapper mapper) {
    this.publicKeyPath = Objects.requireNonNull(publicKeyPath);
    this.mapper = Objects.requireNonNull(mapper);
  }

  @Override
  public String seal(UUID tenantId, UUID providerId, String credential) {
    Objects.requireNonNull(tenantId);
    Objects.requireNonNull(providerId);
    Objects.requireNonNull(credential);
    byte[] dataKey = new byte[32];
    byte[] nonce = new byte[12];
    byte[] plaintext = credential.getBytes(StandardCharsets.UTF_8);
    try {
      random.nextBytes(dataKey);
      random.nextBytes(nonce);
      var publicKey = loadPublicKey();
      var rsa = Cipher.getInstance("RSA/ECB/OAEPPadding");
      rsa.init(Cipher.ENCRYPT_MODE, publicKey, new OAEPParameterSpec(
          "SHA-256", "MGF1", MGF1ParameterSpec.SHA256, PSource.PSpecified.DEFAULT));
      var encryptedKey = rsa.doFinal(dataKey);
      var aes = Cipher.getInstance("AES/GCM/NoPadding");
      aes.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(dataKey, "AES"),
          new GCMParameterSpec(128, nonce));
      aes.updateAAD(aad(tenantId, providerId));
      var ciphertext = aes.doFinal(plaintext);
      var envelope = mapper.createObjectNode();
      envelope.put("schema_version", 1);
      envelope.put("key_id", HexFormat.of().formatHex(
          MessageDigest.getInstance("SHA-256").digest(publicKey.getEncoded())));
      envelope.put("algorithm", ALGORITHM);
      envelope.put("encrypted_key", Base64.getEncoder().encodeToString(encryptedKey));
      envelope.put("nonce", Base64.getEncoder().encodeToString(nonce));
      envelope.put("ciphertext", Base64.getEncoder().encodeToString(ciphertext));
      return mapper.writeValueAsString(envelope);
    } catch (Exception error) {
      throw new IllegalStateException("provider credential could not be sealed", error);
    } finally {
      Arrays.fill(dataKey, (byte) 0);
      Arrays.fill(plaintext, (byte) 0);
    }
  }

  private RSAPublicKey loadPublicKey() throws Exception {
    if (publicKeyPath.isBlank()) {
      throw new IllegalStateException("model gateway credential public key is not configured");
    }
    var pem = Files.readString(Path.of(publicKeyPath), StandardCharsets.US_ASCII);
    var encoded = pem
        .replace("-----BEGIN PUBLIC KEY-----", "")
        .replace("-----END PUBLIC KEY-----", "")
        .replaceAll("\\s", "");
    var key = KeyFactory.getInstance("RSA").generatePublic(
        new X509EncodedKeySpec(Base64.getDecoder().decode(encoded)));
    if (!(key instanceof RSAPublicKey rsa) || rsa.getModulus().bitLength() < 3072) {
      throw new IllegalStateException("model gateway credential key must be RSA-3072 or stronger");
    }
    return rsa;
  }

  private static byte[] aad(UUID tenantId, UUID providerId) {
    return (tenantId + ":" + providerId).getBytes(StandardCharsets.UTF_8);
  }
}
