package com.agentplatform.control.api;

import static org.mockito.Mockito.when;
import static org.springframework.security.test.web.servlet.request.SecurityMockMvcRequestPostProcessors.jwt;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import com.agentplatform.control.run.RunTarget;
import com.agentplatform.control.run.RunTargetService;
import com.agentplatform.control.security.SecurityConfiguration;
import java.util.List;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.context.annotation.Import;
import org.springframework.security.oauth2.jwt.JwtDecoder;
import org.springframework.test.context.TestPropertySource;
import org.springframework.test.web.servlet.MockMvc;

@WebMvcTest(ConsoleRunTargetController.class)
@Import(SecurityConfiguration.class)
@TestPropertySource(properties = "spring.security.user.password=test-scrape-password")
class ConsoleRunTargetControllerTest {
  @Autowired private MockMvc mvc;
  @MockBean private RunTargetService runTargetService;
  @MockBean private JwtDecoder jwtDecoder;

  @Test
  void returnsOnlyTargetsResolvedForTheAuthorizedTenantAndApplication() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var target = new RunTarget(
        UUID.randomUUID(), UUID.randomUUID(), "Local Workspace",
        UUID.randomUUID(), "Local Runtime Agent", 1,
        UUID.randomUUID(), "Native Model Gateway");
    when(runTargetService.available(tenantId, applicationId, 100)).thenReturn(List.of(target));

    mvc.perform(get("/v1/console/run-targets")
            .with(jwt().jwt(jwt -> jwt.subject("operator")
                .claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:write"))))
        .andExpect(status().isOk())
        .andExpect(jsonPath("$.items[0].session_id").value(target.sessionId().toString()))
        .andExpect(jsonPath("$.items[0].workspace_name").value("Local Workspace"))
        .andExpect(jsonPath("$.items[0].agent_version_id").value(target.agentVersionId().toString()))
        .andExpect(jsonPath("$.items[0].agent_name").value("Local Runtime Agent"))
        .andExpect(jsonPath("$.items[0].agent_version").value(1))
        .andExpect(jsonPath("$.items[0].model_policy_name").value("Native Model Gateway"));
  }

  @Test
  void runReaderCannotEnumerateTargetsUsedToCreateRuns() throws Exception {
    mvc.perform(get("/v1/console/run-targets")
            .with(jwt().jwt(jwt -> jwt.subject("reader")
                .claim("tenant_id", UUID.randomUUID().toString())
                .claim("application_id", UUID.randomUUID().toString())
                .claim("scope", "runs:read"))))
        .andExpect(status().isForbidden());
  }
}
