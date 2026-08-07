package com.agentplatform.control.api;

import com.agentplatform.control.run.RunTarget;
import com.agentplatform.control.run.RunTargetService;
import com.agentplatform.control.security.TenantContext;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;
import java.util.UUID;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.security.oauth2.jwt.Jwt;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

@RestController
@RequestMapping("/v1/console")
public class ConsoleRunTargetController {
  private final RunTargetService runTargetService;

  public ConsoleRunTargetController(RunTargetService runTargetService) {
    this.runTargetService = runTargetService;
  }

  @GetMapping("/run-targets")
  RunTargetListResponse availableTargets(@AuthenticationPrincipal Jwt jwt) {
    var context = TenantContext.from(jwt);
    var items = runTargetService.available(context.tenantId(), context.applicationId(), 100).stream()
        .map(RunTargetItem::from)
        .toList();
    return new RunTargetListResponse(items);
  }

  record RunTargetListResponse(List<RunTargetItem> items) {}

  record RunTargetItem(
      @JsonProperty("session_id") UUID sessionId,
      @JsonProperty("workspace_id") UUID workspaceId,
      @JsonProperty("workspace_name") String workspaceName,
      @JsonProperty("agent_version_id") UUID agentVersionId,
      @JsonProperty("agent_name") String agentName,
      @JsonProperty("agent_version") int agentVersion,
      @JsonProperty("model_policy_id") UUID modelPolicyId,
      @JsonProperty("model_policy_name") String modelPolicyName) {
    static RunTargetItem from(RunTarget target) {
      return new RunTargetItem(
          target.sessionId(), target.workspaceId(), target.workspaceName(),
          target.agentVersionId(), target.agentName(), target.agentVersion(),
          target.modelPolicyId(), target.modelPolicyName());
    }
  }
}
