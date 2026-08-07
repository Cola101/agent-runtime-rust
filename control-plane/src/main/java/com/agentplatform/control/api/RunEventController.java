package com.agentplatform.control.api;

import com.agentplatform.control.event.RunEventStreamService;
import com.agentplatform.control.security.TenantContext;
import java.util.UUID;
import org.springframework.http.MediaType;
import org.springframework.security.core.annotation.AuthenticationPrincipal;
import org.springframework.security.oauth2.jwt.Jwt;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;
import org.springframework.web.bind.annotation.RequestHeader;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;
import org.springframework.web.servlet.mvc.method.annotation.SseEmitter;

@RestController
@RequestMapping("/v1/runs")
public class RunEventController {
  private final RunEventStreamService streams;

  public RunEventController(RunEventStreamService streams) {
    this.streams = streams;
  }

  @GetMapping(value = "/{runId}/events", produces = MediaType.TEXT_EVENT_STREAM_VALUE)
  SseEmitter stream(
      @AuthenticationPrincipal Jwt jwt,
      @PathVariable UUID runId,
      @RequestHeader(value = "Last-Event-ID", required = false) UUID lastEventId) {
    var context = TenantContext.from(jwt);
    return streams.open(context.tenantId(), context.applicationId(), runId, lastEventId);
  }
}
