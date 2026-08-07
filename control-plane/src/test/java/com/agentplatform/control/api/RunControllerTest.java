package com.agentplatform.control.api;

import static org.mockito.ArgumentMatchers.eq;
import static org.mockito.Mockito.when;
import static org.springframework.security.test.web.servlet.request.SecurityMockMvcRequestPostProcessors.jwt;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.post;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.header;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import com.agentplatform.control.run.CreateRunCommand;
import com.agentplatform.control.run.Run;
import com.agentplatform.control.run.RunService;
import com.agentplatform.control.run.RunCancellationResult;
import com.agentplatform.control.run.RunStatus;
import com.agentplatform.control.run.RunSummary;
import com.agentplatform.control.run.RunSteeringResult;
import com.agentplatform.control.run.RunSteeringConflict;
import com.agentplatform.control.run.RunSteeringNotAllowed;
import com.agentplatform.control.run.RunSteeringRateLimited;
import com.agentplatform.control.run.SteerRunCommand;
import com.agentplatform.control.security.SecurityConfiguration;
import java.time.Instant;
import java.util.UUID;
import java.time.Duration;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.http.MediaType;
import org.springframework.context.annotation.Import;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.security.oauth2.jwt.JwtDecoder;
import org.springframework.test.context.TestPropertySource;

@WebMvcTest(RunController.class)
@Import(SecurityConfiguration.class)
@TestPropertySource(properties = "spring.security.user.password=test-scrape-password")
class RunControllerTest {
  @Autowired private MockMvc mvc;
  @MockBean private RunService runService;
  @MockBean private JwtDecoder jwtDecoder;

  @Test
  void unauthenticatedCallerCannotCreateRun() throws Exception {
    mvc.perform(post("/v1/sessions/{sessionId}/runs", UUID.randomUUID())
            .header("Idempotency-Key", "request-1")
            .contentType(MediaType.APPLICATION_JSON)
            .content(requestBody(UUID.randomUUID(), UUID.randomUUID())))
        .andExpect(status().isUnauthorized());
  }

  @Test
  void tokenWithoutTenantAndApplicationClaimsIsForbidden() throws Exception {
    mvc.perform(post("/v1/sessions/{sessionId}/runs", UUID.randomUUID())
            .with(jwt().jwt(jwt -> jwt.claim("scope", "runs:write")))
            .header("Idempotency-Key", "request-1")
            .contentType(MediaType.APPLICATION_JSON)
            .content(requestBody(UUID.randomUUID(), UUID.randomUUID())))
        .andExpect(status().isForbidden())
        .andExpect(jsonPath("$.title").value("Tenant context is missing"));
  }

  @Test
  void authenticatedTenantGetsAcceptedRunAndEventsLocation() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var sessionId = UUID.randomUUID();
    var agentVersionId = UUID.randomUUID();
    var workspaceId = UUID.randomUUID();
    var modelPolicyId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var command = new CreateRunCommand(
        sessionId, agentVersionId, workspaceId, modelPolicyId, "hello", 1000, 100, 60);
    var run = new Run(
        runId, tenantId, sessionId, agentVersionId, workspaceId, modelPolicyId, "request-1", "hello",
        RunStatus.QUEUED, 1000, 100, 60, Instant.parse("2026-07-31T00:00:00Z"));
    when(runService.create(eq(tenantId), eq(applicationId), eq("request-1"), eq(command)))
        .thenReturn(run);

    mvc.perform(post("/v1/sessions/{sessionId}/runs", sessionId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "request-1")
            .contentType(MediaType.APPLICATION_JSON)
            .content(requestBody(agentVersionId, workspaceId, modelPolicyId)))
        .andExpect(status().isAccepted())
        .andExpect(header().string("Location", "/v1/runs/" + runId))
        .andExpect(jsonPath("$.run_id").value(runId.toString()))
        .andExpect(jsonPath("$.events_url").value("/v1/runs/" + runId + "/events"));
  }

  @Test
  void tenantCanListOnlyRunsReturnedFromItsAuthorizedContext() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var summary = new RunSummary(
        UUID.randomUUID(), "Workspace A", "Release analyst", RunStatus.RUNNING,
        12000, 500, 3600, Instant.parse("2026-07-31T00:00:00Z"));
    when(runService.recent(tenantId, applicationId, 50)).thenReturn(List.of(summary));

    mvc.perform(get("/v1/runs")
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:read"))))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.items[0].workspace_name").value("Workspace A"))
        .andExpect(jsonPath("$.items[0].status").value("running"))
        .andExpect(jsonPath("$.items[0].budget.max_tokens").value(12000));
  }

  @Test
  void authenticatedTenantCanRequestRunCancellation() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(runService.cancel(tenantId, applicationId, runId))
        .thenReturn(new RunCancellationResult(runId, RunStatus.RUNNING));

    mvc.perform(post("/v1/runs/{runId}:cancel", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write"))))
        .andExpect(status().isAccepted())
        .andExpect(jsonPath("$.run_id").value(runId.toString()))
        .andExpect(jsonPath("$.status").value("running"));
  }

  @Test
  void authenticatedTenantCanDurablySteerARunningRun() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var steeringId = UUID.randomUUID();
    var command = new SteerRunCommand("Focus on the authorization failure first.");
    when(runService.steer(tenantId, applicationId, runId, "steer-1", command))
        .thenReturn(new RunSteeringResult(runId, steeringId, "pending"));

    mvc.perform(post("/v1/runs/{runId}:steer", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "steer-1")
            .contentType(MediaType.APPLICATION_JSON)
            .content("""
                {"input":"Focus on the authorization failure first."}
                """))
        .andExpect(status().isAccepted())
        .andExpect(jsonPath("$.run_id").value(runId.toString()))
        .andExpect(jsonPath("$.steering_id").value(steeringId.toString()))
        .andExpect(jsonPath("$.state").value("pending"));
  }

  @Test
  void unsafeSteeringBoundaryIsAConflictInsteadOfAnInternalFailure() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(runService.steer(
        tenantId,
        applicationId,
        runId,
        "steer-unsafe",
        new SteerRunCommand("continue")))
        .thenThrow(new RunSteeringNotAllowed(runId));

    mvc.perform(post("/v1/runs/{runId}:steer", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "steer-unsafe")
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"input\":\"continue\"}"))
        .andExpect(status().isConflict())
        .andExpect(jsonPath("$.title").value("Run cannot be steered now"));
  }

  @Test
  void steeringRateLimitReturnsRetryAfterWithoutHidingTheRunBoundary() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(runService.steer(
        tenantId,
        applicationId,
        runId,
        "steer-too-fast",
        new SteerRunCommand("continue")))
        .thenThrow(new RunSteeringRateLimited(runId, Duration.ofMillis(1_200)));

    mvc.perform(post("/v1/runs/{runId}:steer", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "steer-too-fast")
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"input\":\"continue\"}"))
        .andExpect(status().isTooManyRequests())
        .andExpect(header().string("Retry-After", "2"))
        .andExpect(jsonPath("$.title").value("Run steering rate limit exceeded"))
        .andExpect(jsonPath("$.retry_after_seconds").value(2));
  }

  @Test
  void changedInputWithTheSameSteeringKeyIsAConflict() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    when(runService.steer(
        tenantId,
        applicationId,
        runId,
        "steer-conflict",
        new SteerRunCommand("different")))
        .thenThrow(new RunSteeringConflict(runId));

    mvc.perform(post("/v1/runs/{runId}:steer", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "steer-conflict")
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"input\":\"different\"}"))
        .andExpect(status().isConflict())
        .andExpect(jsonPath("$.title").value("Run steering conflicts with an existing command"));
  }

  @Test
  void steeringRejectsInputThatExceedsTheUtf8ByteLimit() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();

    mvc.perform(post("/v1/runs/{runId}:steer", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write")))
            .header("Idempotency-Key", "steer-too-large")
            .contentType(MediaType.APPLICATION_JSON)
            .content("{\"input\":\"" + "界".repeat(11_000) + "\"}"))
        .andExpect(status().isBadRequest())
        .andExpect(jsonPath("$.title").value("Run steering request is invalid"));
  }

  private String requestBody(UUID agentVersionId, UUID workspaceId) {
    return requestBody(agentVersionId, workspaceId, UUID.randomUUID());
  }

  private String requestBody(UUID agentVersionId, UUID workspaceId, UUID modelPolicyId) {
    return """
        {
          "agent_version_id": "%s",
          "workspace_id": "%s",
          "model_policy_id": "%s",
          "input": "hello",
          "budget": {
            "max_tokens": 1000,
            "max_cost_cents": 100,
            "max_duration_seconds": 60
          }
        }
        """.formatted(agentVersionId, workspaceId, modelPolicyId);
  }
}
