package com.agentplatform.control.resource;

import static org.junit.jupiter.api.Assertions.assertDoesNotThrow;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.verifyNoInteractions;

import java.util.List;
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
}
