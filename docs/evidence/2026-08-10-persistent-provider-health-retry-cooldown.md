# Persistent Provider health and retry evidence — 2026-08-10

## Same-Provider retry and audit proof

- A real OpenAI-compatible loopback endpoint returned HTTP 503 before any
  stream event, then succeeded on the second request. The fallback endpoint
  observed no TCP connection.
- The elapsed delay met the frozen backoff policy. The durable event stream
  contains `model.provider.retry_scheduled`, and route journal schema 2 stores
  the attempt ordinal and retry deadline without the raw error body or API key.
- A Provider invocation interrupted after its in-flight marker was persisted is
  counted as one ambiguous attempt. With a total-attempt budget of one, two
  replacement daemons made no second request and ended the Run explicitly.

## Cooldown and `Retry-After` proof

- With a failure threshold of one, a 503 opened cooldown and the fallback
  completed the first Run. A replacement Host on the same state root completed
  a second Run through the fallback while the primary request count remained
  exactly one.
- A separate primary returned HTTP 429 with `Retry-After: 2` while the ordinary
  threshold was eight. The durable health entry opened immediately, contained
  only classification/status/deadline data, and a replacement Run again made no
  primary connection. Neither the Provider message nor credential appeared in
  the file.
- HTTP authentication failures were executed twice through the same primary,
  never crossed to the fallback and created no shared health file. This proves
  they neither masquerade as availability failures nor contaminate cooldown.

## Half-open concurrency proof

After a 503 cooldown expired, two independent Runs and two Host instances used
the same state root concurrently. The primary held its half-open response open.
While that lease was active, the other Run completed through the fallback. The
primary observed exactly the initial failure plus one probe; after success its
health entry was removed. This test runs under the supported single-writer
state-root process and does not claim cross-process active-active filesystem
locking.

## Recovery compatibility proof

Existing daemon, root Session, context compaction and subagent crash fixtures
now opt into a two-attempt policy when their declared contract is to retry one
ambiguous model request after replacement. Their candidate journal preserves
the consumed attempt across the crash, while completed Tool, child-result,
message and Session receipts are still reused rather than replayed.

## Validation

- Standalone Host tests: 94 passed, 0 failed.
- Full Rust workspace: 453 passed, 0 failed, with 5 external live tests
  explicitly ignored; 458 tests total.
- `cargo check --workspace --all-targets`, Clippy over
  `workspace/all-targets/all-features` with `-D warnings`, Rust formatting and
  diff checks passed.

No Docker, Java, PostgreSQL, NATS, external daemon or external API key was
started or required. Loopback peers used real HTTP/SSE sockets and deterministic
responses; this is protocol/runtime evidence, not live-vendor quality or
billing acceptance.
