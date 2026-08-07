package com.agentplatform.control.api;

import com.agentplatform.control.run.CreateRunCommand;
import com.agentplatform.control.run.RunService;
import com.agentplatform.control.run.RunSummary;
import com.agentplatform.control.run.SteerRunCommand;
import com.agentplatform.control.security.TenantContext;
import com.fasterxml.jackson.annotation.JsonProperty;
import jakarta.validation.Valid;
import jakarta.validation.constraints.NotBlank;
import jakarta.validation.constraints.NotNull;
import jakarta.validation.constraints.Positive;
import java.net.URI;
import java.util.UUID;
import java.util.List;
import org.springframework.http.ResponseEntity;
import org.springframework.security.oauth2.jwt.Jwt;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestHeader;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/v1")
public class RunController {
  private final RunService runService;

  public RunController(RunService runService) {
    this.runService = runService;
  }

  @GetMapping("/runs")
  RunListResponse listRuns(@AuthenticationPrincipal Jwt jwt) {
    var context = TenantContext.from(jwt);
    var items = runService.recent(context.tenantId(), context.applicationId(), 50).stream()
        .map(RunListItem::from).toList();
    return new RunListResponse(items);
  }

  @PostMapping("/sessions/{sessionId}/runs")
  ResponseEntity<CreateRunResponse> createRun(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID sessionId,
      @RequestHeader("Idempotency-Key") String idempotencyKey,
      @Valid @RequestBody CreateRunRequest request) {
    var context = TenantContext.from(jwt);
    var run = runService.create(
        context.tenantId(),
        context.applicationId(),
        idempotencyKey,
        new CreateRunCommand(
            sessionId,
            request.agentVersionId(),
            request.workspaceId(),
            request.modelPolicyId(),
            request.input(),
            request.budget().maxTokens(),
            request.budget().maxCostCents(),
            request.budget().maxDurationSeconds()));
    var eventsUrl = "/v1/runs/" + run.id() + "/events";
    return ResponseEntity.accepted()
        .location(URI.create("/v1/runs/" + run.id()))
        .body(new CreateRunResponse(run.id(), eventsUrl));
  }

  @PostMapping("/runs/{runId}:cancel")
  ResponseEntity<CancelRunResponse> cancelRun(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID runId) {
    var context = TenantContext.from(jwt);
    var result = runService.cancel(context.tenantId(), context.applicationId(), runId);
    return ResponseEntity.accepted().body(new CancelRunResponse(
        result.runId(), result.status().name().toLowerCase()));
  }

  @PostMapping("/runs/{runId}:steer")
  ResponseEntity<SteerRunResponse> steerRun(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID runId,
      @RequestHeader("Idempotency-Key") String idempotencyKey,
      @Valid @RequestBody SteerRunRequest request) {
    var context = TenantContext.from(jwt);
    var result = runService.steer(
        context.tenantId(),
        context.applicationId(),
        runId,
        idempotencyKey,
        new SteerRunCommand(request.input()));
    return ResponseEntity.accepted().body(new SteerRunResponse(
        result.runId(), result.steeringId(), result.state()));
  }

  record CreateRunRequest(
      @NotNull @JsonProperty("agent_version_id") UUID agentVersionId,
      @NotNull @JsonProperty("workspace_id") UUID workspaceId,
      @NotNull @JsonProperty("model_policy_id") UUID modelPolicyId,
      @NotBlank String input,
      @NotNull @Valid BudgetRequest budget) {}

  record BudgetRequest(
      @Positive @JsonProperty("max_tokens") long maxTokens,
      @Positive @JsonProperty("max_cost_cents") long maxCostCents,
      @Positive @JsonProperty("max_duration_seconds") long maxDurationSeconds) {}

  record CreateRunResponse(
      @JsonProperty("run_id") UUID runId,
      @JsonProperty("events_url") String eventsUrl) {}

  record CancelRunResponse(
      @JsonProperty("run_id") UUID runId,
      String status) {}

  record SteerRunRequest(@NotBlank String input) {}

  record SteerRunResponse(
      @JsonProperty("run_id") UUID runId,
      @JsonProperty("steering_id") UUID steeringId,
      String state) {}

  record RunListResponse(List<RunListItem> items) {}

  record RunListItem(
      UUID id,
      @JsonProperty("workspace_name") String workspaceName,
      @JsonProperty("agent_name") String agentName,
      String status,
      BudgetResponse budget,
      @JsonProperty("created_at") String createdAt) {
    static RunListItem from(RunSummary summary) {
      return new RunListItem(
          summary.id(), summary.workspaceName(), summary.agentName(),
          summary.status().name().toLowerCase(),
          new BudgetResponse(summary.maxTokens(), summary.maxCostCents(), summary.maxDurationSeconds()),
          summary.createdAt().toString());
    }
  }

  record BudgetResponse(
      @JsonProperty("max_tokens") long maxTokens,
      @JsonProperty("max_cost_cents") long maxCostCents,
      @JsonProperty("max_duration_seconds") long maxDurationSeconds) {}
}
