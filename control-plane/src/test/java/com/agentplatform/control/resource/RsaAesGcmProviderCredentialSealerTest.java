package com.agentplatform.control.resource;

import static org.assertj.core.api.Assertions.assertThat;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.interfaces.RSAPublicKey;
import java.security.spec.MGF1ParameterSpec;
import java.util.Base64;
import java.util.UUID;
import javax.crypto.Cipher;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.OAEPParameterSpec;
import javax.crypto.spec.PSource;
import javax.crypto.spec.SecretKeySpec;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;
import java.nio.file.Path;

class RsaAesGcmProviderCredentialSealerTest {
  @TempDir Path temporary;

  @Test
  void sealsAProviderCredentialForTheGatewayWithoutPersistingPlaintext() throws Exception {
    var keyPairGenerator = KeyPairGenerator.getInstance("RSA");
    keyPairGenerator.initialize(3072);
    var keyPair = keyPairGenerator.generateKeyPair();
    var publicKey = (RSAPublicKey) keyPair.getPublic();
    var publicKeyPath = temporary.resolve("model-credential-public.pem");
    Files.writeString(publicKeyPath, pem("PUBLIC KEY", publicKey.getEncoded()));
    var mapper = new ObjectMapper();
    var sealer = new RsaAesGcmProviderCredentialSealer(publicKeyPath.toString(), mapper);
    var tenantId = UUID.randomUUID();
    var providerId = UUID.randomUUID();
    var plaintext = "tenant-secret-that-must-never-be-returned";

    var envelope = sealer.seal(tenantId, providerId, plaintext);
    var json = mapper.readTree(envelope);

    assertThat(envelope).doesNotContain(plaintext);
    assertThat(json.path("schema_version").asInt()).isEqualTo(1);
    assertThat(json.path("algorithm").asText()).isEqualTo("RSA-OAEP-256+A256GCM");
    assertThat(json.path("key_id").asText()).isEqualTo(
        hex(MessageDigest.getInstance("SHA-256").digest(publicKey.getEncoded())));

    var rsa = Cipher.getInstance("RSA/ECB/OAEPPadding");
    rsa.init(Cipher.DECRYPT_MODE, keyPair.getPrivate(), new OAEPParameterSpec(
        "SHA-256", "MGF1", MGF1ParameterSpec.SHA256, PSource.PSpecified.DEFAULT));
    var dataKey = rsa.doFinal(Base64.getDecoder().decode(json.path("encrypted_key").asText()));
    var aes = Cipher.getInstance("AES/GCM/NoPadding");
    aes.init(Cipher.DECRYPT_MODE, new SecretKeySpec(dataKey, "AES"),
        new GCMParameterSpec(128, Base64.getDecoder().decode(json.path("nonce").asText())));
    aes.updateAAD((tenantId + ":" + providerId).getBytes(StandardCharsets.UTF_8));

    assertThat(new String(
        aes.doFinal(Base64.getDecoder().decode(json.path("ciphertext").asText())),
        StandardCharsets.UTF_8)).isEqualTo(plaintext);
  }

  private static String pem(String type, byte[] encoded) {
    return "-----BEGIN " + type + "-----\n"
        + Base64.getMimeEncoder(64, new byte[] {'\n'}).encodeToString(encoded)
        + "\n-----END " + type + "-----\n";
  }

  private static String hex(byte[] bytes) {
    return java.util.HexFormat.of().formatHex(bytes);
  }
}
