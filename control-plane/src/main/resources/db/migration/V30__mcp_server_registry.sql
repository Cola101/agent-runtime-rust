-- Tenant registry of federated MCP servers (ADR-0040).
--
-- Registered like a model Provider, not like a Tool. A Tool here is a binary we
-- built, registered, pinned by digest and re-validated before every spawn
-- (ADR-0025). None of that applies to third-party code chosen by a tenant, and
-- pretending it does would mean the digest guarantee quietly stops meaning what
-- it says everywhere else.
--
-- What a Provider row already gives us, and the reason this table looks like
-- one: tenant scoping, a sealed credential the Worker never sees in plaintext,
-- and an endpoint that is the only host the client may reach for this server.
--
-- v1 is HTTP only. There is no command column and no argument column, because
-- there is nothing to spawn: a local stdio server would be arbitrary third-party
-- code running on the Worker, and the Seatbelt profile emits no network rule at
-- all while network access is the entire point of an MCP server. Adding those
-- columns "for later" would invite exactly the implementation this ADR rejects.

create table mcp_servers (
  tenant_id uuid not null,
  id uuid not null,
  application_id uuid not null,
  name varchar(200) not null,
  endpoint varchar(2048) not null,
  credential_envelope jsonb,
  state varchar(20) not null default 'active',
  created_at timestamptz not null default now(),
  updated_at timestamptz not null default now(),
  primary key (tenant_id, id),
  constraint mcp_servers_application_fk foreign key (tenant_id, application_id)
    references applications (tenant_id, id),
  constraint mcp_servers_name_unique unique (tenant_id, application_id, name),
  constraint mcp_servers_state_check check (state in ('active', 'disabled')),
  -- The name becomes the namespace in `mcp:<server>/<tool>`. A name carrying a
  -- slash or a colon could produce a qualified tool name that parses as a
  -- different server, so the shape is constrained at the only place that cannot
  -- be bypassed.
  constraint mcp_servers_name_shape_check check (name ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
  -- A credential is optional -- some servers are open -- but if present it must
  -- be a sealed envelope rather than a bare string, so a plaintext key cannot be
  -- stored by writing to the wrong field.
  constraint mcp_servers_credential_object_check check (
    credential_envelope is null or jsonb_typeof(credential_envelope) = 'object')
);

comment on table mcp_servers is
  'Tenant-registered MCP servers federated over HTTP (ADR-0040). No local process is ever spawned from this row.';
comment on column mcp_servers.name is
  'Namespace in qualified tool names: mcp:<name>/<tool>. Constrained so a name can never make one server''s tool parse as another''s.';
comment on column mcp_servers.endpoint is
  'The only host the federation client may reach for this server. Egress stays enumerable per tenant, which is what makes it auditable.';

create index mcp_servers_application_idx on mcp_servers (tenant_id, application_id)
  where state = 'active';

-- `force` as well as `enable`: without it the table owner bypasses the policy,
-- and the control plane connects as the owner. Every other tenant-scoped table
-- here does both, and a registry of third-party endpoints is the last place to
-- deviate from that.
alter table mcp_servers enable row level security;
alter table mcp_servers force row level security;
create policy tenant_isolation on mcp_servers
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
