alter table run_checkpoints alter column payload drop not null;

alter table run_checkpoints
  add column payload_ref text,
  add column payload_encoding varchar(16),
  add column stored_payload_digest varchar(64),
  add column uncompressed_size bigint,
  add column stored_size bigint;

update run_checkpoints
   set payload_encoding = 'identity',
       stored_payload_digest = payload_digest,
       uncompressed_size = octet_length(payload),
       stored_size = octet_length(payload);

alter table run_checkpoints
  alter column payload_encoding set not null,
  alter column stored_payload_digest set not null,
  alter column uncompressed_size set not null,
  alter column stored_size set not null;

alter table run_checkpoints drop constraint run_checkpoints_schema_check;
alter table run_checkpoints drop constraint run_checkpoints_payload_size_check;

alter table run_checkpoints
  add constraint run_checkpoints_schema_check check (schema_version in (1, 2)),
  add constraint run_checkpoints_stored_payload_digest_check check (
    stored_payload_digest ~ '^[0-9a-f]{64}$'),
  add constraint run_checkpoints_payload_encoding_check check (
    (schema_version = 1 and payload_encoding = 'identity')
    or (schema_version = 2 and payload_encoding = 'zstd')),
  add constraint run_checkpoints_payload_location_check check (
    (schema_version = 1 and payload is not null and payload_ref is null
      and payload_digest = stored_payload_digest
      and uncompressed_size = stored_size)
    or (schema_version = 2 and (
      (payload is not null and payload_ref is null)
      or (payload is null and payload_ref is not null)))),
  add constraint run_checkpoints_payload_ref_check check (
    payload_ref is null
    or payload_ref = 'checkpoint://sha256/' || stored_payload_digest),
  add constraint run_checkpoints_payload_size_check check (
    uncompressed_size between 1 and 16777216
    and stored_size between 1 and 17825792
    and (payload is null or (
      octet_length(payload) = stored_size and octet_length(payload) <= 524288)));
