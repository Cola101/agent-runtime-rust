package com.agentplatform.control.approval;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HashSet;
import java.util.Optional;
import java.util.TreeMap;

public record ToolApprovalScope(
    String toolName,
    String effect,
    String sandbox,
    String policyDigest,
    String sessionScopeDigest,
    JsonNode policySnapshot,
    boolean portableSessionScope) {
  private static final ObjectMapper JSON = new ObjectMapper();

  public static Optional<ToolApprovalScope> parse(JsonNode approval) {
    var snapshot = approval.get("policy_snapshot");
    var policyDigest = approval.path("policy_digest").asText();
    var scopeDigest = approval.path("session_scope_digest").asText();
    var hasSnapshot = snapshot != null && !snapshot.isNull();
    var hasPolicyDigest = !policyDigest.isBlank();
    var hasScopeDigest = !scopeDigest.isBlank();
    if (!hasSnapshot && !hasPolicyDigest && !hasScopeDigest) {
      return Optional.empty();
    }
    if (!hasSnapshot || !hasPolicyDigest || !hasScopeDigest || !snapshot.isObject()
        || !policyDigest.matches("[0-9a-f]{64}")
        || !scopeDigest.matches("[0-9a-f]{64}")) {
      throw new IllegalArgumentException("approval policy snapshot is incomplete");
    }

    var execution = approval.path("execution");
    var call = execution.path("call");
    var arguments = call.path("arguments");
    var toolName = requiredText(snapshot, "tool_name", 256);
    var effect = requiredText(snapshot, "effect", 32);
    var sandbox = requiredText(snapshot, "sandbox", 32);
    if (!toolName.equals(call.path("name").asText())
        || !effect.equals(execution.path("effect").asText())
        || !sandbox.equals(execution.path("sandbox").asText())
        || !"ask".equals(snapshot.path("approval").asText())
        || !snapshot.path("implementation_digest").asText().matches("[0-9a-f]{64}")
        || !arguments.isObject()) {
      throw new IllegalArgumentException("approval policy snapshot does not match the tool call");
    }
    validateScopes(snapshot.path("required_scopes"));
    if (!sha256(snapshot).equals(policyDigest)) {
      throw new IllegalArgumentException("approval policy digest does not match its snapshot");
    }
    var portableSessionScope = hasPortableNumbers(arguments);
    if (portableSessionScope) {
      var scope = JSON.createObjectNode();
      scope.set("arguments", arguments);
      scope.set("policy_snapshot", snapshot);
      scope.put("tool_name", toolName);
      if (!sha256(scope).equals(scopeDigest)) {
        throw new IllegalArgumentException("approval session scope digest does not match its binding");
      }
    }
    return Optional.of(new ToolApprovalScope(
        toolName, effect, sandbox, policyDigest, scopeDigest, snapshot.deepCopy(),
        portableSessionScope));
  }

  public boolean sessionGrantEligible() {
    return portableSessionScope && ("pure".equals(effect) || "idempotent".equals(effect));
  }

  private static String requiredText(JsonNode object, String field, int maxLength) {
    var value = object.path(field).asText();
    if (value.isBlank() || value.length() > maxLength) {
      throw new IllegalArgumentException("approval policy snapshot has invalid " + field);
    }
    return value;
  }

  private static void validateScopes(JsonNode scopes) {
    if (!(scopes instanceof ArrayNode)) {
      throw new IllegalArgumentException("approval policy snapshot has invalid required_scopes");
    }
    var seen = new HashSet<String>();
    String previous = null;
    for (var scope : scopes) {
      if (!scope.isTextual() || scope.asText().isBlank() || !seen.add(scope.asText())
          || (previous != null && previous.compareTo(scope.asText()) > 0)) {
        throw new IllegalArgumentException(
            "approval policy snapshot required_scopes must be sorted and unique");
      }
      previous = scope.asText();
    }
  }

  private static boolean hasPortableNumbers(JsonNode value) {
    if (value.isNumber()) {
      return value.isIntegralNumber();
    }
    if (value.isContainerNode()) {
      for (var child : value) {
        if (!hasPortableNumbers(child)) {
          return false;
        }
      }
    }
    return true;
  }

  private static String sha256(JsonNode value) {
    try {
      return java.util.HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(
          JSON.writeValueAsString(canonicalize(value)).getBytes(StandardCharsets.UTF_8)));
    } catch (NoSuchAlgorithmException | JsonProcessingException exception) {
      throw new IllegalStateException("cannot compute approval policy digest", exception);
    }
  }

  private static JsonNode canonicalize(JsonNode value) {
    if (value.isObject()) {
      var sorted = new TreeMap<String, JsonNode>();
      value.fields().forEachRemaining(entry -> sorted.put(entry.getKey(), entry.getValue()));
      ObjectNode canonical = JSON.createObjectNode();
      sorted.forEach((key, child) -> canonical.set(key, canonicalize(child)));
      return canonical;
    }
    if (value.isArray()) {
      ArrayNode canonical = JSON.createArrayNode();
      value.forEach(child -> canonical.add(canonicalize(child)));
      return canonical;
    }
    return value.deepCopy();
  }
}
