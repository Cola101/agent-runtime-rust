# ADR-0039: Auto-approval for provably read-only shell commands

## Status

Accepted. Narrows ADR-0038 decision 8.

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
