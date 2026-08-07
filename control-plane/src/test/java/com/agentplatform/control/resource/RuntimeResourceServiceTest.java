package com.agentplatform.control.resource;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.verifyNoInteractions;

import java.util.List;
import java.util.Map;
import java.util.UUID;
import org.junit.jupiter.api.Test;

class RuntimeResourceServiceTest {
  @Test
  void rejectsAgentInstructionsThatExceedTheRuntimeUtf8ByteLimit() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "界".repeat(10_667), List.of("tool:workspace.read")));

    verifyNoInteractions(repository);
  }

  @Test
  void rejectsAOneComponentSkillVersionBeforeSigningOrPersistence() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.publishSkillVersion(
        UUID.randomUUID(), UUID.randomUUID(), "review", "0", "Review files",
        "Read evidence.", List.of(), List.of("darwin-arm64"), "1.0.0"));

    verifyNoInteractions(repository);
  }

  @Test
  void rejectsDuplicateSkillToolDeclarationsBeforeSigningOrPersistence() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.publishSkillVersion(
        UUID.randomUUID(), UUID.randomUUID(), "review", "1.0.0", "Review files",
        "Read evidence.", List.of("workspace.read_text", "workspace.read_text"),
        List.of("darwin-arm64"), "0.1.0"));

    verifyNoInteractions(repository);
  }

  @Test
  void rejectsASubagentRoleWhoseScopesExceedTheParentAgentVersion() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Coordinate a bounded review.", List.of(), List.of(),
        List.of(new SubagentRoleDefinition(
            "reviewer", "Review evidence without modifying it.",
            List.of("tool:workspace.read")))));

    verifyNoInteractions(repository);
  }

  @Test
  void acceptsTheWorkspaceWriteScopeTheWorkerNowInstalls() {
    // The Worker installs workspace.write_text (ADR-0036). A Tool the Worker can
    // run but the control plane refuses to delegate is unreachable, so the two
    // allowlists have to agree.
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertDoesNotThrow(() -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Author findings into the workspace.",
        List.of("tool:workspace.read", "tool:workspace.write", "tool:shell.exec"),
        List.of(), List.of()));
  }

  @Test
  void stillRejectsAScopeNoTrustedToolDeclares() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Do anything.", List.of("tool:workspace.delete"), List.of(), List.of()));
  }

  // The approval policy is the tenant's decision, so the tenant has to be able
  // to state it. It shipped as a Worker constant, which meant every tenant
  // granted a Tool got the same exemption and none could turn it off.
  @Test
  void acceptsAToolApprovalPolicyForAToolTheAgentVersionActuallyDelegates() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertDoesNotThrow(() -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Inspect the workspace.",
        List.of("tool:shell.exec"),
        List.of(), List.of(),
        Map.of("shell.exec", "provably_read_only_shell_command")));
  }

  // A policy naming a Tool this AgentVersion cannot reach is either a mistake or
  // an attempt to pre-authorise something a later change would activate. Either
  // way it must not be stored as though it meant something.
  @Test
  void rejectsAPolicyForAToolTheAgentVersionCannotReach() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Inspect the workspace.",
        List.of("tool:workspace.read"),
        List.of(), List.of(),
        Map.of("shell.exec", "provably_read_only_shell_command")));
  }

  // An unrecognised policy name must fail rather than fall back to something.
  // A permissive fallback is how a typo becomes a grant.
  @Test
  void rejectsAnUnrecognisedPolicyNameRatherThanDefaultingIt() {
    var repository = mock(RuntimeResourceRepository.class);
    var service = new RuntimeResourceService(repository);

    assertThrows(IllegalArgumentException.class, () -> service.createAgentVersion(
        UUID.randomUUID(), UUID.randomUUID(), UUID.randomUUID(),
        "Inspect the workspace.",
        List.of("tool:shell.exec"),
        List.of(), List.of(),
        Map.of("shell.exec", "trust_me")));
  }
}
