package com.agentplatform.control.api;

import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.when;
import static org.springframework.security.test.web.servlet.request.SecurityMockMvcRequestPostProcessors.jwt;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.asyncDispatch;
import static org.springframework.test.web.servlet.request.MockMvcRequestBuilders.get;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.content;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.jsonPath;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.request;
import static org.springframework.test.web.servlet.result.MockMvcResultMatchers.status;

import com.agentplatform.control.event.EventCursorNotFound;
import com.agentplatform.control.event.RunEventStreamService;
import com.agentplatform.control.security.SecurityConfiguration;
import java.util.UUID;
import org.junit.jupiter.api.Test;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.autoconfigure.web.servlet.WebMvcTest;
import org.springframework.boot.test.mock.mockito.MockBean;
import org.springframework.context.annotation.Import;
import org.springframework.security.oauth2.jwt.JwtDecoder;
import org.springframework.test.context.TestPropertySource;
import org.springframework.test.web.servlet.MockMvc;
import org.springframework.web.servlet.mvc.method.annotation.SseEmitter;

@WebMvcTest(RunEventController.class)
@Import(SecurityConfiguration.class)
@TestPropertySource(properties = "spring.security.user.password=test-scrape-password")
class RunEventControllerTest {
  @Autowired private MockMvc mvc;
  @MockBean private RunEventStreamService streams;
  @MockBean private JwtDecoder jwtDecoder;

  @Test
  void lastEventIdIsPassedToAuthorizedTenantStream() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var lastEventId = UUID.randomUUID();
    var nextEventId = UUID.randomUUID();
    var emitter = new SseEmitter();
    emitter.send(SseEmitter.event()
        .id(nextEventId.toString())
        .name("model.text_delta")
        .data("{\"text\":\"hello\"}"));
    emitter.complete();
    when(streams.open(tenantId, applicationId, runId, lastEventId)).thenReturn(emitter);

    var result = mvc.perform(get("/v1/runs/{runId}/events", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:read")))
            .header("Last-Event-ID", lastEventId))
        .andExpect(request().asyncStarted())
        .andReturn();

    mvc.perform(asyncDispatch(result))
        .andExpect(status().isOk())
        .andExpect(content().string(org.hamcrest.Matchers.containsString("id:" + nextEventId)))
        .andExpect(content().string(org.hamcrest.Matchers.containsString("event:model.text_delta")));
    verify(streams).open(tenantId, applicationId, runId, lastEventId);
  }

  @Test
  void unknownLastEventIdIsRejectedAsClientErrorSoEventSourceStopsReconnecting() throws Exception {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    var runId = UUID.randomUUID();
    var unknownEventId = UUID.randomUUID();
    when(streams.open(tenantId, applicationId, runId, unknownEventId))
        .thenThrow(new EventCursorNotFound(unknownEventId));

    // A 5xx tells a browser EventSource the server is at fault, so it reconnects
    // with the same unusable cursor forever. An unknown cursor is client input.
    mvc.perform(get("/v1/runs/{runId}/events", runId)
            .with(jwt().jwt(jwt -> jwt.claim("tenant_id", tenantId.toString())
                .claim("application_id", applicationId.toString())
                .claim("scope", "runs:read")))
            .header("Last-Event-ID", unknownEventId))
        .andExpect(status().isNotFound())
        .andExpect(jsonPath("$.type").value("urn:agent-runtime:problem:event-cursor"));
  }
}
