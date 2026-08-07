package com.agentplatform.control.identity;

public interface WorkloadTokenIssuer {
  WorkloadToken issue(WorkloadIdentityClaims claims);
}
