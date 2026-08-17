/// Stand-in data until the gRPC client lands.
///
/// Kept in one file, apart from every view, so wiring the real client is a
/// matter of replacing this module's exports — not of hunting literals through
/// the surfaces.
import type { Blocked, Run } from "./surfaces/model";

export const runs: Run[] = [
  { id: "r1", title: "Retention scan budget", lifecycle: { kind: "waiting_approval" }, tokens: 6240, costCents: 31, when: "2m" },
  { id: "r2", title: "EPERM on session close", lifecycle: { kind: "terminal", status: "indeterminate" }, tokens: 2100, costCents: 8, when: "18m" },
  { id: "r3", title: "mTLS handshake trace", lifecycle: { kind: "terminal", status: "succeeded" }, tokens: 8800, costCents: 44, when: "1h" },
  { id: "r4", title: "Event cursor paging", lifecycle: { kind: "retired", status: "succeeded" }, tokens: 3900, costCents: 19, when: "3h" },
  { id: "r5", title: "Operator token shape", lifecycle: { kind: "running" }, tokens: 410, costCents: 2, when: "now" },
];

export const blocked: Blocked[] = [
  {
    kind: "approval", runId: "r1", runTitle: "Retention scan budget",
    command: "cargo test -p agent-runtime-host --test embedded_retention",
    digest: "4f2c9ae1…b73d0c",
  },
  {
    kind: "indeterminate", runId: "r2", runTitle: "EPERM on session close",
    question: "The signal was sent and the reply was lost. Did that process group die?",
  },
];

export const identity = {
  who: "ops@acme",
  tenant: "acme",
  application: "desk",
  expiresInMinutes: 41,
};
