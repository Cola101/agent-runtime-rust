alter table outbox_events
  add column claim_token uuid,
  add column claim_until timestamptz,
  add column last_error varchar(2000);

alter table outbox_events
  add constraint outbox_claim_pair_check check (
    (claim_token is null and claim_until is null) or
    (claim_token is not null and claim_until is not null)
  );

create index outbox_claimable_idx on outbox_events (claim_until, created_at, id)
  where published_at is null;
