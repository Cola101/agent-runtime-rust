package com.agentplatform.control.api;

import static org.mockito.ArgumentMatchers.any;
import static org.mockito.ArgumentMatchers.eq;
import static org.mockito.Mockito.when;
import static org.springframework.security.test.web.servlet.request.SecurityMockMvcRequestPostProcessors.jwt;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import com.agentplatform.control.approval.Approval;
import com.agentplatform.control.approval.ApprovalConflict;
import com.agentplatform.control.approval.ApprovalDecision;
import com.agentplatform.control.approval.ApprovalService;
import com.agentplatform.control.approval.ApprovalStatus;
import com.agentplatform.control.approval.ApprovalSummary;
import com.agentplatform.control.security.SecurityConfiguration;
import com.fasterxml.jackson.databind.ObjectMapper;
import java.time.Instant;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.context.annotation.Import;
import org.springframework.http.MediaType;
import org.springframework.security.oauth2.jwt.JwtDecoder;
import org.springframework.test.context.TestPropertySource;
import org.springframework.test.web.servlet.MockMvc;

@WebMvcTest(ApprovalController.class)
@Import(SecurityConfiguration.class)
@TestPropertySource(properties = "spring.security.user.password=test-scrape-password")
class ApprovalControllerTest {
  @Autowired private MockMvc mvc;
  @MockBean private ApprovalService approvalService;
  @MockBean private JwtDecoder jwtDecoder;

  @Test
  void authenticatedReviewerListsOnlyPendingApprovalsForTheCurrentApplication() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var approvalId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(approvalService.pending(tenantId, applicationId, 25)).thenReturn(List.of(
        new ApprovalSummary(
            approvalId, runId, 1, ApprovalStatus.PENDING,
            "Release workspace", "Release analyst", "workspace.read_text", "call_readme",
            "pure", "trusted_native", "a".repeat(64),
            new ObjectMapper().readTree("{\"path\":\"README.md\"}"),
            Instant.parse("2026-08-02T04:00:00Z"))));

    mvc.perform(get("/v1/approvals")
            .queryParam("status", "pending")
            .queryParam("limit", "25")
            .with(jwt().jwt(jwt -> jwt.subject("reviewer-7")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "approvals:read"))))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.items.length()").value(1))
        .andExpect(jsonPath("$.items[0].id").value(approvalId.toString()))
        .andExpect(jsonPath("$.items[0].run_id").value(runId.toString()))
        .andExpect(jsonPath("$.items[0].workspace_name").value("Release workspace"))
        .andExpect(jsonPath("$.items[0].agent_name").value("Release analyst"))
        .andExpect(jsonPath("$.items[0].tool_name").value("workspace.read_text"))
        .andExpect(jsonPath("$.items[0].tool_call_id").value("call_readme"))
        .andExpect(jsonPath("$.items[0].effect").value("pure"))
        .andExpect(jsonPath("$.items[0].sandbox").value("trusted_native"))
        .andExpect(jsonPath("$.items[0].arguments.path").value("README.md"))
        .andExpect(jsonPath("$.items[0].available_decisions[0]").value("allow_once"))
        .andExpect(jsonPath("$.items[0].available_decisions[1]").value("deny"))
        .andExpect(jsonPath("$.items[0].version").value(1))
        .andExpect(jsonPath("$.items[0].status").value("pending"));
  }

  @Test
  void replaySafePolicyBoundApprovalAdvertisesSessionGrant() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var approvalId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var json = new ObjectMapper();
    var policySnapshot = json.readTree("""
        {"approval":"ask","effect":"pure","implementation_digest":"%s","required_scopes":["workspace:read"],"sandbox":"trusted_native","tool_name":"workspace.read_text"}
        """.formatted("d".repeat(64)));
    when(approvalService.pending(tenantId, applicationId, 25)).thenReturn(List.of(
        new ApprovalSummary(
            approvalId, runId, 1, ApprovalStatus.PENDING,
            "Release workspace", "Release analyst", "workspace.read_text", "call_readme",
            "pure", "trusted_native", "a".repeat(64),
            json.readTree("{\"path\":\"README.md\"}"),
            Instant.parse("2026-08-02T04:00:00Z"), "b".repeat(64), "c".repeat(64),
            policySnapshot, true)));

    mvc.perform(get("/v1/approvals")
            .queryParam("status", "pending")
            .queryParam("limit", "25")
            .with(jwt().jwt(jwt -> jwt.subject("reviewer-7")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "approvals:read"))))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.items[0].available_decisions[0]").value("allow_once"))
        .andExpect(jsonPath("$.items[0].available_decisions[1]").value("allow_session"))
        .andExpect(jsonPath("$.items[0].available_decisions[2]").value("deny"))
        .andExpect(jsonPath("$.items[0].policy_digest").value("b".repeat(64)))
        .andExpect(jsonPath("$.items[0].session_scope_digest").value("c".repeat(64)))
        .andExpect(jsonPath("$.items[0].policy_snapshot.tool_name")
            .value("workspace.read_text"));
  }

  @Test
  void reviewerCanGrantAReplaySafeToolForTheBoundSessionScope() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var approvalId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(approvalService.decide(
        eq(tenantId), eq(applicationId), eq(approvalId), eq(1),
        any(ApprovalDecision.class), eq("same command in this session"), eq("reviewer-7")))
        .thenReturn(new Approval(
            approvalId, tenantId, runId, 2, ApprovalStatus.APPROVED,
            Instant.parse("2026-08-01T04:00:00Z")));

    mvc.perform(post("/v1/approvals/{approvalId}:decide", approvalId)
            .with(jwt().jwt(jwt -> jwt.subject("reviewer-7")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "approvals:write")))
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"version":1,"decision":"allow_session","reason":"same command in this session"}
                """))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.status").value("approved"));
  }

  @Test
  void authenticatedReviewerDecidesTheTenantApprovalAtTheExpectedVersion() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var approvalId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(approvalService.decide(
        tenantId, applicationId, approvalId, 1,
        ApprovalDecision.ALLOW_ONCE, "reviewed", "reviewer-7"))
        .thenReturn(new Approval(
            approvalId, tenantId, runId, 2, ApprovalStatus.APPROVED,
            Instant.parse("2026-08-01T04:00:00Z")));

    mvc.perform(post("/v1/approvals/{approvalId}:decide", approvalId)
            .with(jwt().jwt(jwt -> jwt.subject("reviewer-7")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "approvals:write")))
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"version":1,"decision":"allow_once","reason":"reviewed"}
                """))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.id").value(approvalId.toString()))
        .andExpect(jsonPath("$.runId").value(runId.toString()))
        .andExpect(jsonPath("$.version").value(2))
        .andExpect(jsonPath("$.status").value("approved"));
  }

  @Test
  void staleApprovalVersionIsReportedAsConflict() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var approvalId = UUID.randomUUID();
    when(approvalService.decide(
        tenantId, applicationId, approvalId, 1,
        ApprovalDecision.DENY, null, "reviewer-7"))
        .thenThrow(new ApprovalConflict(approvalId));

    mvc.perform(post("/v1/approvals/{approvalId}:decide", approvalId)
            .with(jwt().jwt(jwt -> jwt.subject("reviewer-7")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "approvals:write")))
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"version":1,"decision":"deny"}
                """))
        .andExpect(status().isConflict())
        .andExpect(jsonPath("$.title").value("Approval is stale or no longer pending"));
  }

  @Test
  void authenticatedCallerWithoutApprovalWriteScopeIsForbidden() throws Exception {
    mvc.perform(post("/v1/approvals/{approvalId}:decide", UUID.randomUUID())
            .with(jwt().jwt(jwt -> jwt.subject("reader-3")
                .claim("tenant_id", UUID.randomUUID().toString())
                .claim("application_id", UUID.randomUUID().toString())
                .claim("scope", "runs:read")))
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"version":1,"decision":"deny"}
                """))
        .andExpect(status().isForbidden());
  }

  @Test
  void approvalListRequiresApprovalReadScope() throws Exception {
    mvc.perform(get("/v1/approvals")
            .with(jwt().jwt(jwt -> jwt.subject("runner-3")
                .claim("tenant_id", UUID.randomUUID().toString())
                .claim("application_id", UUID.randomUUID().toString())
                .claim("scope", "runs:read approvals:write"))))
        .andExpect(status().isForbidden());
  }

  @Test
  void approvalListRejectsUnsupportedStatusAndOversizedPage() throws Exception {
    var authorized = jwt().jwt(jwt -> jwt.subject("reviewer-7")
        .claim("tenant_id", UUID.randomUUID().toString())
        .claim("application_id", UUID.randomUUID().toString())
        .claim("scope", "approvals:read"));

    mvc.perform(get("/v1/approvals")
            .queryParam("status", "approved")
            .with(authorized))
        .andExpect(status().isBadRequest());
    mvc.perform(get("/v1/approvals")
            .queryParam("limit", "101")
            .with(authorized))
        .andExpect(status().isBadRequest());
  }
}
