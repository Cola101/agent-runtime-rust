package com.agentplatform.control.resource;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.assertThatThrownBy;
import static org.assertj.core.api.Assertions.failBecauseExceptionWasNotThrown;
import static org.mockito.ArgumentMatchers.any;
import static org.mockito.ArgumentMatchers.anyString;
import static org.mockito.Mockito.mock;
import static org.mockito.Mockito.never;
import static org.mockito.Mockito.verify;
import static org.mockito.Mockito.when;

import java.time.Instant;
import java.util.UUID;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.mockito.ArgumentCaptor;

/**
 * Registration rules for federated MCP servers (ADR-0040).
 *
 * <p>These are the checks that decide what a tenant is allowed to point the
 * platform at, so they run against the service rather than the database: a
 * constraint that only exists in SQL is a constraint the API can return a 500
 * for instead of a refusal.
 */
class McpServerRegistryTest {
  private RuntimeResourceRepository repository;
  private ProviderCredentialSealer sealer;
  private RuntimeResourceService service;

  @BeforeEach
  void setUp() {
    repository = mock(RuntimeResourceRepository.class);
    sealer = mock(ProviderCredentialSealer.class);
    when(sealer.seal(any(), any(), anyString())).thenReturn("{\"sealed\":true}");
    service = new RuntimeResourceService(repository, sealer);
  }

  @Test
  void registeringAServerSealsTheCredentialBeforeItReachesTheRepository() {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    when(repository.createMcpServer(any(), any(), any(), anyString(), anyString(), any()))
        .thenReturn(new McpServerResource(
            UUID.randomUUID(), "search", "https://mcp.example.com/rpc", "active",
            "sealed", Instant.now()));

    service.createMcpServer(tenantId, applicationId, "search",
        "https://mcp.example.com/rpc", "secret-token");

    var envelope = ArgumentCaptor.forClass(String.class);
    verify(repository).createMcpServer(
        any(), any(), any(), anyString(), anyString(), envelope.capture());
    assertThat(envelope.getValue()).doesNotContain("secret-token");
  }

  // The name becomes the namespace in `mcp:<server>/<tool>`. A name carrying a
  // separator could produce a qualified name that parses as a different server.
  @Test
  void aServerNameCannotCarryTheQualifiedNameSeparators() {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();

    for (var hostile : new String[] {"a/b", "a:b", "mcp:evil", "UPPER", "", "a b", "-lead"}) {
      // Written out rather than assertThatThrownBy in a loop: that form fails
      // without saying which input was accepted, which is the one thing the
      // failure needs to tell you.
      try {
        service.createMcpServer(
            tenantId, applicationId, hostile, "https://mcp.example.com/rpc", null);
        failBecauseExceptionWasNotThrown(IllegalArgumentException.class);
      } catch (IllegalArgumentException expected) {
        assertThat(expected).hasMessageContaining("name");
      } catch (AssertionError accepted) {
        throw new AssertionError("server name \"" + hostile + "\" was accepted", accepted);
      }
    }
    verify(repository, never()).createMcpServer(
        any(), any(), any(), anyString(), anyString(), any());
  }

  // Surrounding whitespace is trimmed, not refused -- every other name field
  // here behaves that way, and a trimmed name is unambiguous. Pinned because it
  // is a real decision either way, and silence would leave it to whoever reads
  // the regex next.
  @Test
  void aServerNameIsTrimmedRatherThanRejectedForSurroundingSpace() {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    when(repository.createMcpServer(any(), any(), any(), anyString(), anyString(), any()))
        .thenReturn(new McpServerResource(
            UUID.randomUUID(), "search", "https://mcp.example.com/rpc", "active",
            "absent", Instant.now()));

    service.createMcpServer(
        tenantId, applicationId, "  search  ", "https://mcp.example.com/rpc", null);

    var name = ArgumentCaptor.forClass(String.class);
    verify(repository).createMcpServer(
        any(), any(), any(), name.capture(), anyString(), any());
    assertThat(name.getValue()).isEqualTo("search");
  }

  // Egress is the point of the endpoint field: it is the only host the client
  // may reach for this server. Plain HTTP to an arbitrary host would send a
  // tenant's sealed credential over the wire in clear.
  @Test
  void aServerEndpointMustBeHttpsOrLoopback() {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();

    assertThatThrownBy(() -> service.createMcpServer(
        tenantId, applicationId, "search", "http://mcp.example.com/rpc", null))
        .isInstanceOf(IllegalArgumentException.class);
    assertThatThrownBy(() -> service.createMcpServer(
        tenantId, applicationId, "search", "https://user:pass@mcp.example.com/rpc", null))
        .isInstanceOf(IllegalArgumentException.class);
    verify(repository, never()).createMcpServer(
        any(), any(), any(), anyString(), anyString(), any());
  }

  // Open servers exist. Requiring a credential would make them unregisterable
  // and push tenants towards inventing a placeholder secret.
  @Test
  void aServerWithoutACredentialIsAllowedAndSealsNothing() {
    var tenantId = UUID.randomUUID();
    var applicationId = UUID.randomUUID();
    when(repository.createMcpServer(any(), any(), any(), anyString(), anyString(), any()))
        .thenReturn(new McpServerResource(
            UUID.randomUUID(), "open", "https://mcp.example.com/rpc", "active",
            "absent", Instant.now()));

    var created = service.createMcpServer(
        tenantId, applicationId, "open", "https://mcp.example.com/rpc", null);

    assertThat(created.credentialStatus()).isEqualTo("absent");
    verify(sealer, never()).seal(any(), any(), anyString());
  }
}
