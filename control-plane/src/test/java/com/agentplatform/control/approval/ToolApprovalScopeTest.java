package com.agentplatform.control.approval;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;

class ToolApprovalScopeTest {
  private static final ObjectMapper JSON = new ObjectMapper();

  @Test
  void legacyApprovalWithoutPolicyFieldsRemainsAllowOnceCompatible() throws Exception {
    var approval = JSON.readTree("""
        {"execution":{"call":{"id":"call-1","name":"workspace.read_text","arguments":{"path":"README.md"}},"effect":"pure","sandbox":"trusted_native"}}
        """);

    assertThat(ToolApprovalScope.parse(approval)).isEmpty();
  }

  @Test
  void changedArgumentsCannotReuseAClaimedSessionScopeDigest() throws Exception {
    var policy = """
        {"approval":"ask","effect":"pure","implementation_digest":"%s","required_scopes":["workspace:read"],"sandbox":"trusted_native","tool_name":"workspace.read_text"}
        """.formatted("d".repeat(64)).strip();
    var approvedScope = """
        {"arguments":{"path":"README.md"},"policy_snapshot":%s,"tool_name":"workspace.read_text"}
        """.formatted(policy).strip();
    var approval = JSON.readTree("""
        {"execution":{"call":{"id":"call-2","name":"workspace.read_text","arguments":{"path":"SECURITY.md"}},"effect":"pure","sandbox":"trusted_native"},"policy_snapshot":%s,"policy_digest":"%s","session_scope_digest":"%s"}
        """.formatted(policy, sha256(policy), sha256(approvedScope)));

    assertThatThrownBy(() -> ToolApprovalScope.parse(approval))
        .isInstanceOf(IllegalArgumentException.class)
        .hasMessageContaining("session scope digest");
  }

  @Test
  void floatingPointArgumentsRemainAllowOnceOnlyUntilCanonicalizationIsPortable() throws Exception {
    var policy = """
        {"approval":"ask","effect":"pure","implementation_digest":"%s","required_scopes":["workspace:read"],"sandbox":"trusted_native","tool_name":"workspace.read_text"}
        """.formatted("d".repeat(64)).strip();
    var approval = JSON.readTree("""
        {"execution":{"call":{"id":"call-3","name":"workspace.read_text","arguments":{"ratio":1.5}},"effect":"pure","sandbox":"trusted_native"},"policy_snapshot":%s,"policy_digest":"%s","session_scope_digest":"%s"}
        """.formatted(policy, sha256(policy), "c".repeat(64)));

    assertThat(ToolApprovalScope.parse(approval)).hasValueSatisfying(
        scope -> assertThat(scope.sessionGrantEligible()).isFalse());
  }

  private static String sha256(String value) throws Exception {
    return HexFormat.of().formatHex(
        MessageDigest.getInstance("SHA-256").digest(value.getBytes(StandardCharsets.UTF_8)));
  }
}
