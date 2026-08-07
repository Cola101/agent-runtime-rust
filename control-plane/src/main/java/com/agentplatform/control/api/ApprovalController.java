package com.agentplatform.control.api;

import com.agentplatform.control.approval.Approval;
import com.agentplatform.control.approval.ApprovalDecision;
import com.agentplatform.control.approval.ApprovalService;
import com.agentplatform.control.approval.ApprovalSummary;
import com.agentplatform.control.security.TenantContext;
import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.JsonNode;
import jakarta.validation.Valid;
import jakarta.validation.constraints.Max;
import jakarta.validation.constraints.Min;
import jakarta.validation.constraints.NotNull;
import jakarta.validation.constraints.Size;
import java.util.UUID;
import java.util.List;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.security.oauth2.jwt.Jwt;
import org.springframework.http.HttpStatus;
import org.springframework.web.server.ResponseStatusException;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RequestParam;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/v1/approvals")
public class ApprovalController {
  private final ApprovalService approvals;

  public ApprovalController(ApprovalService approvals) {
    this.approvals = approvals;
  }

  @GetMapping
  ApprovalListResponse listPending(
      @AuthenticationPrincipal Jwt jwt,
      @RequestParam(defaultValue = "pending") String status,
      @RequestParam(defaultValue = "50") int limit) {
    if (!"pending".equals(status) || limit < 1 || limit > 100) {
      throw new ResponseStatusException(
          HttpStatus.BAD_REQUEST, "status must be pending and limit must be between 1 and 100");
    }
    var context = TenantContext.from(jwt);
    return new ApprovalListResponse(approvals.pending(
        context.tenantId(), context.applicationId(), limit).stream()
        .map(ApprovalListItem::from).toList());
  }

  @PostMapping("/{approvalId}:decide")
  Approval decide(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID approvalId,
      @Valid @RequestBody ApprovalDecisionRequest request) {
    var context = TenantContext.from(jwt);
    return approvals.decide(
        context.tenantId(), context.applicationId(), approvalId,
        request.version(), request.decision(), request.reason(),
        jwt.getSubject());
  }

  record ApprovalDecisionRequest(
      @Min(1) @Max(Integer.MAX_VALUE) int version,
      @NotNull ApprovalDecision decision,
      @Size(max = 1000) String reason) {}

  record ApprovalListResponse(List<ApprovalListItem> items) {}

  record ApprovalListItem(
      UUID id,
      @JsonProperty("run_id") UUID runId,
      int version,
      String status,
      @JsonProperty("workspace_name") String workspaceName,
      @JsonProperty("agent_name") String agentName,
      @JsonProperty("tool_name") String toolName,
      @JsonProperty("tool_call_id") String toolCallId,
      String effect,
      String sandbox,
      @JsonProperty("binding_digest") String bindingDigest,
      JsonNode arguments,
      @JsonProperty("policy_digest") String policyDigest,
      @JsonProperty("session_scope_digest") String sessionScopeDigest,
      @JsonProperty("policy_snapshot") JsonNode policySnapshot,
      @JsonProperty("available_decisions") List<String> availableDecisions,
      @JsonProperty("created_at") String createdAt) {
    static ApprovalListItem from(ApprovalSummary summary) {
      return new ApprovalListItem(
          summary.id(), summary.runId(), summary.version(), summary.status().value(),
          summary.workspaceName(), summary.agentName(), summary.toolName(), summary.toolCallId(),
          summary.effect(), summary.sandbox(), summary.bindingDigest(), summary.arguments(),
          summary.policyDigest(), summary.sessionScopeDigest(), summary.policySnapshot(),
          summary.sessionGrantEligible()
              ? List.of("allow_once", "allow_session", "deny")
              : List.of("allow_once", "deny"),
          summary.createdAt().toString());
    }
  }
}
