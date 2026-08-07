# ADR-0039: Auto-approval for provably read-only shell commands

## Status

**Withdrawn 2026-08-07, the day it was accepted.** The exemption is off:
`shell.exec` declares `AutoApproval::Never` in both the Worker and the local
host, so every call asks again.

A review found the allow-list called five writable commands read-only, which
made this an approval bypass rather than a convenience:

| Command | What it does |
| --- | --- |
| `git branch -D` | deletes a branch |
| `git tag -d` | deletes a tag |
| `git diff --output=f` | writes a file; `log` and `show` take it too |
| `uniq in out` | second positional is an output file |
| `file -C` | compiles and writes a magic database |

Two further defects the same review found, both now fixed:

- The binding digest did not cover `approval` or `auto_approval`, so a gated
  call and an exempted one produced identical digests.
- The exempt path returned a bare `ToolPlan::Execute` and dropped the policy
  snapshot and digest, which made decision 3 below — that the exemption stays
  auditable — **untrue in the code**. There is now a distinct
  `ToolPlan::AutoApproved` and a `tool.execution.auto_approved` event carrying
  the snapshot, its digest and a stated reason.

The mechanism was salvageable and is kept. The judgement that produced the list
was not: see "What went wrong" below. Read the decisions that follow as the
design of a mechanism that is currently disabled, not as current behaviour.

## What went wrong

The list carried this comment: *executables with no write capability reachable
through any flag*. I verified that claim for `sort`, `sed`, `tee` and `awk`, and
then wrote it as though it held for every entry without checking the rest.

The wording is what hid two of the five. `uniq`'s output is a **positional
argument**, not a flag, so a rule phrased around flags could not see it. And
git's write modes live in **subcommands whose names read as queries**, plus a
shared diff-option surface that includes `--output=`, so enumerating "read-only
subcommands" cannot be correct while that surface exists. `git` is now off the
list entirely for that reason.

A replacement heuristic — refuse any command with more than one positional
operand — was tried and removed within the hour: it read `head -n 20 file` as two
operands, because telling an option's value from an operand needs the
per-command knowledge the heuristic existed to avoid. That was the original
mistake in a new shape.

The durable lesson is the one ADR-0038 already implied and this ADR failed to
apply to itself: **a curated list is only as good as the review behind it**, and
a list is the wrong place for a security decision that a tenant should be making.
Re-enabling this requires the policy to be a tenant decision carried in the
execution snapshot, not a constant in the Worker.

## The precondition is now met (2026-08-07, later the same day)

The policy is no longer a Worker constant. A tenant sets
`tool_approval_policies` on an AgentVersion; it is stored in the version's spec,
projected by the Scheduler into RunExecution v8, and applied by the kernel from
the command. `auto_approval` was removed from `ToolDescriptor` entirely so there
is one source rather than two.

Absent means ask, enforced independently at each layer -- the API record
defaults a missing map, the command contract treats an absent Tool as gated, and
the kernel's lookup falls back to `Never` -- so the safe reading survives any one
layer being bypassed. Two contract rules guard the edges: an unrecognised policy
value fails to decode rather than defaulting, and a command claiming a pre-v8
schema while carrying the v8 field is refused as a downgrade. Subagents receive
an empty map unconditionally, because a role-scoped exemption is a second
decision nobody has made.

**This does not re-enable anything.** No Tool declares a policy anywhere, so
every `shell.exec` call is still approval gated. What changed is who is able to
decide. Turning it on additionally requires a list that survives the review this
one did not, and that list does not exist yet.

## Superseded content

Everything below is the original ADR as accepted, kept because the mechanism
survives and its reasoning is still the reasoning.

## Context

ADR-0038 made every `shell.exec` call approval-gated, and recorded the cost in
its own Limitations:

> **没有命令白名单**，每条命令都要审批。真实工作里这很重。

That understates it. A person asked to approve `ls`, then `wc -l`, then
`git status` does not evaluate the tenth request; they click through it, or they
turn the gate off. An approval flow that is always on is an approval flow that
stops being read, which is worse than a narrower one that is.

The question is which calls can be exempted without giving anything away. The
useful reframing is to ask what an approval still protects, given what is
already blocked. After ADR-0036 and ADR-0037 the container prevents writes
outside the Workspace, all outbound network, and reads of the credential
directories. What remains is: **the user's own files inside the Workspace**.

So a command that cannot write anything has nothing left for a human to decide.

Codex reaches auto-approval the same way — through the action being constrained
rather than through a name being trusted (`core/src/safety.rs`, where
`SafetyCheck::AutoApprove` follows from the patch being confined to writable
paths). Its general mechanism is a whole policy language, `codex-execpolicy`,
with a Starlark parser, per-rule matching, network rules and prefix rules. That
breadth serves its Tool surface; here there is one Tool and one question.

## Decision

1. **Exemption is declared per Tool, never inferred.** `ToolDescriptor` carries
   `auto_approval: AutoApproval`, defaulting to `Never`. A Tool that says
   nothing keeps asking every time, which is what every Tool did before this
   existed.

2. **The Worker does not decide this on its own.** The exemption travels with
   the Tool definition, and the kernel applies it only when the descriptor
   declares it. A Worker that could exempt calls by itself would be deciding
   tenant authorization, which ADR-0001 forbids.

3. **The exemption is recorded in the policy snapshot**, so the approval ledger
   shows which exemption was in force rather than only that no approval was
   asked for. An auto-approved call must not be indistinguishable from a Tool
   that was never gated.

4. **Only `AutoApproval::ProvablyReadOnlyShellCommand` exists**, and only
   `shell.exec` declares it. The file Tools stay `Never`: `workspace.write_text`
   writes by definition, and `workspace.read_text` is already so narrow that an
   exemption would buy nothing.

5. **The classifier answers two ways, and everything uncertain is "ask".**
   A third value ("probably fine") would be a place for uncertainty to hide. The
   cost of a wrong `ask` is a click; the cost of a wrong `read-only` is an
   unreviewed command with the Tool's full capability.

6. **No shell parser.** A parser would have to be right about quoting,
   expansion, precedence and word splitting, and being subtly wrong about any of
   those is how this kind of code grants what it meant to withhold. The accepted
   grammar is deliberately tiny:

   - separators `|`, `&&`, `||`, `;`, and **every** segment must pass on its own;
   - words are bare, or wholly inside single quotes, where sh expands nothing;
   - any other shell character asks: `> < & $ ` ( ) { } \ " ~ * ? [ ] ! #`;
   - the executable must be a bare name, never a path, because a path is not the
     name it looks like.

7. **The allow-list contains only executables with no write capability
   reachable through any flag.** `sort -o`, `sed -i`, `tee` and `awk` are absent
   for that reason: including them would require per-flag rules, and a per-flag
   rule that misses one flag is a silent write.

8. **`git` is the one exception**, allowed for read-only subcommands only, and
   only when nothing precedes the subcommand. `git -c alias.x='!rm -rf /' x` is
   a write capability wearing a read's name.

## Consequences

### Positive

- The common case — reading files, counting lines, `git status` — runs without
  interrupting anyone, so the approvals that remain are ones worth reading.
- Adding a second exemption forces a decision about which Tool it applies to,
  because `AutoApproval` is an enum rather than a boolean.

### Negative

- The allow-list is small enough to be frustrating. `find`, `sed`, `awk`,
  `sort`, double quotes and globs all ask. That is the intended direction of
  error, but it is a real cost.
- The list is fixed in code. There is no per-tenant configuration, and adding a
  command is a code change and a release.
- A command can still read anything the user can read that is not a credential
  directory, and now it can do so without anyone seeing the command. This is the
  exemption's real price and it is not hypothetical.

### Neutral

- `ToolDescriptor` and `ToolApprovalPolicySnapshot` both gained a field, so
  every construction site had to state its policy. That churn is the point: a
  default would have made silent exemptions possible.

## Failure Modes

- Unparseable command: asks.
- Unknown executable: asks.
- Any segment of a chain not read-only: the whole command asks.
- Classifier disagrees with the shell about word boundaries: it only ever
  under-approves, because anything it cannot represent is rejected.

## Limitations

- **Reads are not reviewed.** An exempted command can read the Workspace and
  most of the filesystem, and no human sees it. Reducing this needs read
  containment beyond the credential directories, which ADR-0037 records as
  impractical at this layer.
- Not verified against a shell other than `/bin/sh`.
- The classifier has no notion of arguments for the allowed commands, so
  `cat /etc/passwd` is exempt. Contained, but unreviewed.
- No telemetry on how often the exemption fires, so there is no way yet to tell
  whether the list is too small in practice.

## Alternatives Considered

- **Keep asking for everything:** rejected as the status quo whose cost ADR-0038
  already recorded. An unread approval is not a control.
- **Port `codex-execpolicy`:** rejected for now. It is a policy language and a
  parser for a Tool surface much wider than one shell. Worth revisiting when
  there are more Tools to govern.
- **Let the model declare a command read-only:** rejected outright. The model is
  the untrusted party in this design.
- **Allow-list by prefix (`git *`, `ls *`):** rejected. Prefix matching on a
  string that can contain `;` is the first thing that breaks.

## References

- ADR-0036 Seatbelt-contained trusted Tools and the first Workspace write
- ADR-0037 sensitive-path read containment
- ADR-0038 the Shell Tool
- Codex `codex-rs/core/src/safety.rs`, `codex-rs/core/src/exec_policy.rs`
