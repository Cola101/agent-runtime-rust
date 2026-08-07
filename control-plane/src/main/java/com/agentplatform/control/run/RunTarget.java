package com.agentplatform.control.run;

import java.util.UUID;

public record RunTarget(
    UUID sessionId,
    UUID workspaceId,
    String workspaceName,
    UUID agentVersionId,
    String agentName,
    int agentVersion,
    UUID modelPolicyId,
    String modelPolicyName) {}
