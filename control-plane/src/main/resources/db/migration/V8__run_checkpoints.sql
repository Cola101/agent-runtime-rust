create table run_checkpoints (
  tenant_id uuid not null,
  checkpoint_id uuid not null,
  run_id uuid not null,
  session_id uuid not null,
  attempt_id uuid not null,
  owner_epoch bigint not null,
  fencing_token uuid not null,
  sequence bigint not null,
  status varchar(32) not null,
  schema_version integer not null,
  kernel_digest varchar(64) not null,
  tool_catalog_digest varchar(64) not null,
  payload bytea not null,
  payload_digest varchar(64) not null,
  created_at timestamptz not null,
  primary key (tenant_id, checkpoint_id),
  constraint run_checkpoints_dispatch_fk
    foreign key (tenant_id, run_id, attempt_id)
    references run_dispatches (tenant_id, run_id, attempt_id),
  constraint run_checkpoints_sequence_unique
    unique (tenant_id, run_id, attempt_id, sequence),
  constraint run_checkpoints_epoch_check check (owner_epoch > 0),
  constraint run_checkpoints_sequence_check check (sequence >= 0),
  constraint run_checkpoints_schema_check check (schema_version = 1),
  constraint run_checkpoints_status_check check (
    status in ('running', 'waiting_approval', 'suspended')),
  constraint run_checkpoints_kernel_digest_check check (
    kernel_digest ~ '^[0-9a-f]{64}$'),
  constraint run_checkpoints_tool_catalog_digest_check check (
    tool_catalog_digest ~ '^[0-9a-f]{64}$'),
  constraint run_checkpoints_payload_digest_check check (
    payload_digest ~ '^[0-9a-f]{64}$'),
  constraint run_checkpoints_payload_size_check check (
    octet_length(payload) between 1 and 16777216)
);

create index run_checkpoints_latest_idx
  on run_checkpoints (tenant_id, run_id, attempt_id, sequence desc);

alter table run_checkpoints enable row level security;
alter table run_checkpoints force row level security;
create policy tenant_isolation on run_checkpoints
  using (tenant_id = current_tenant_id())
  with check (tenant_id = current_tenant_id());
