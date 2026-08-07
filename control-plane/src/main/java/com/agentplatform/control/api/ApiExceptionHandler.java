package com.agentplatform.control.api;

import com.agentplatform.control.security.TenantContextMissing;
import com.agentplatform.control.approval.ApprovalConflict;
import com.agentplatform.control.approval.ApprovalDecisionNotAllowed;
import com.agentplatform.control.approval.ApprovalNotFound;
import com.agentplatform.control.event.EventCursorNotFound;
import com.agentplatform.control.run.RunAlreadyTerminal;
import com.agentplatform.control.run.InvalidRunSteering;
import com.agentplatform.control.run.RunNotFound;
import com.agentplatform.control.run.RunTargetNotFound;
import com.agentplatform.control.run.RunSteeringConflict;
import com.agentplatform.control.run.RunSteeringNotAllowed;
import com.agentplatform.control.run.RunSteeringRateLimited;
import com.agentplatform.control.resource.ResourceConflict;
import com.agentplatform.control.resource.ResourceParentNotFound;
import java.net.URI;
import java.util.Map;
import org.springframework.http.HttpStatus;
import org.springframework.http.HttpHeaders;
import org.springframework.http.MediaType;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.ExceptionHandler;
import org.springframework.web.bind.annotation.RestControllerAdvice;

@RestControllerAdvice
public class ApiExceptionHandler {
  @ExceptionHandler(TenantContextMissing.class)
  ResponseEntity<Map<String, Object>> missingTenant(TenantContextMissing error) {
    return ResponseEntity.status(HttpStatus.FORBIDDEN)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:tenant-context").toString(),
            "title", "Tenant context is missing",
            "status", HttpStatus.FORBIDDEN.value(),
            "detail", error.getMessage()));
  }

  /**
   * An unknown {@code Last-Event-ID} is client input, not a server fault. Left
   * unmapped it surfaced as 500, and a browser {@code EventSource} treats 5xx as
   * retryable: it reconnects with the same unusable cursor indefinitely. The
   * dedicated type lets a client tell "cursor is stale, re-read from the start"
   * apart from "the Run is gone, stop".
   */
  @ExceptionHandler(EventCursorNotFound.class)
  ResponseEntity<Map<String, Object>> eventCursorNotFound(EventCursorNotFound error) {
    return ResponseEntity.status(HttpStatus.NOT_FOUND)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:event-cursor").toString(),
            "title", "Event cursor was not found",
            "status", HttpStatus.NOT_FOUND.value(),
            "detail", error.getMessage()));
  }

  /**
   * Input the service refused.
   *
   * <p>Left unmapped it surfaced as 500, which tells a client to retry something
   * that will never succeed and files a validation failure as a server incident.
   * The message is the service's own, and those are written for the caller --
   * "this AgentVersion does not delegate that tool" is exactly what they need.
   */
  @ExceptionHandler(IllegalArgumentException.class)
  ResponseEntity<Map<String, Object>> invalidResourceInput(IllegalArgumentException error) {
    return ResponseEntity.status(HttpStatus.BAD_REQUEST)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:resource-input").toString(),
            "title", "Request was refused",
            "status", HttpStatus.BAD_REQUEST.value(),
            "detail", error.getMessage() == null ? "invalid request" : error.getMessage()));
  }

  @ExceptionHandler(RunAlreadyTerminal.class)
  ResponseEntity<Map<String, Object>> runAlreadyTerminal(RunAlreadyTerminal error) {
    return problem(HttpStatus.CONFLICT, "Run is already terminal", error.getMessage());
  }

  @ExceptionHandler(RunNotFound.class)
  ResponseEntity<Map<String, Object>> runNotFound(RunNotFound error) {
    return problem(HttpStatus.NOT_FOUND, "Run was not found", error.getMessage());
  }

  @ExceptionHandler(RunTargetNotFound.class)
  ResponseEntity<Map<String, Object>> runTargetNotFound(RunTargetNotFound error) {
    return problem(HttpStatus.NOT_FOUND, "Run target was not found", error.getMessage());
  }

  @ExceptionHandler(RunSteeringNotAllowed.class)
  ResponseEntity<Map<String, Object>> runSteeringNotAllowed(RunSteeringNotAllowed error) {
    return problem(HttpStatus.CONFLICT, "Run cannot be steered now", error.getMessage());
  }

  @ExceptionHandler(RunSteeringRateLimited.class)
  ResponseEntity<Map<String, Object>> runSteeringRateLimited(RunSteeringRateLimited error) {
    return ResponseEntity.status(HttpStatus.TOO_MANY_REQUESTS)
        .header(HttpHeaders.RETRY_AFTER, Long.toString(error.retryAfterSeconds()))
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:run-steering-rate-limit").toString(),
            "title", "Run steering rate limit exceeded",
            "status", HttpStatus.TOO_MANY_REQUESTS.value(),
            "detail", error.getMessage(),
            "retry_after_seconds", error.retryAfterSeconds()));
  }

  @ExceptionHandler(InvalidRunSteering.class)
  ResponseEntity<Map<String, Object>> invalidRunSteering(InvalidRunSteering error) {
    return problem(HttpStatus.BAD_REQUEST, "Run steering request is invalid", error.getMessage());
  }

  @ExceptionHandler(RunSteeringConflict.class)
  ResponseEntity<Map<String, Object>> runSteeringConflict(RunSteeringConflict error) {
    return problem(
        HttpStatus.CONFLICT,
        "Run steering conflicts with an existing command",
        error.getMessage());
  }

  @ExceptionHandler(ResourceParentNotFound.class)
  ResponseEntity<Map<String, Object>> resourceParentNotFound(ResourceParentNotFound error) {
    return resourceProblem(HttpStatus.NOT_FOUND, "Resource parent was not found", error.getMessage());
  }

  @ExceptionHandler(ResourceConflict.class)
  ResponseEntity<Map<String, Object>> resourceConflict(ResourceConflict error) {
    return resourceProblem(HttpStatus.CONFLICT, "Resource already exists", error.getMessage());
  }

  @ExceptionHandler(ApprovalConflict.class)
  ResponseEntity<Map<String, Object>> approvalConflict(ApprovalConflict error) {
    return approvalProblem(
        HttpStatus.CONFLICT, "Approval is stale or no longer pending", error.getMessage());
  }

  @ExceptionHandler(ApprovalNotFound.class)
  ResponseEntity<Map<String, Object>> approvalNotFound(ApprovalNotFound error) {
    return approvalProblem(HttpStatus.NOT_FOUND, "Approval was not found", error.getMessage());
  }

  @ExceptionHandler(ApprovalDecisionNotAllowed.class)
  ResponseEntity<Map<String, Object>> approvalDecisionNotAllowed(
      ApprovalDecisionNotAllowed error) {
    return approvalProblem(HttpStatus.CONFLICT, "Approval decision is not available", error.getMessage());
  }

  private ResponseEntity<Map<String, Object>> problem(
      HttpStatus status, String title, String detail) {
    return ResponseEntity.status(status)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:run").toString(),
            "title", title,
            "status", status.value(),
            "detail", detail));
  }

  private ResponseEntity<Map<String, Object>> approvalProblem(
      HttpStatus status, String title, String detail) {
    return ResponseEntity.status(status)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:approval").toString(),
            "title", title,
            "status", status.value(),
            "detail", detail));
  }

  private ResponseEntity<Map<String, Object>> resourceProblem(
      HttpStatus status, String title, String detail) {
    return ResponseEntity.status(status)
        .contentType(MediaType.APPLICATION_PROBLEM_JSON)
        .body(Map.of(
            "type", URI.create("urn:agent-runtime:problem:resource").toString(),
            "title", title,
            "status", status.value(),
            "detail", detail));
  }
}
