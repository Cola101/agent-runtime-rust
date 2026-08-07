//! Classifies a shell command as provably read-only, or not.
//!
//! Only two answers, and everything uncertain is the second one. The cost of a
//! wrong `ask` is a click; the cost of a wrong `read-only` is an unreviewed
//! command with the Tool's full capability.
//!
//! ## Why this is narrow rather than clever
//!
//! Codex solves the general version of this with a policy language
//! (`codex-execpolicy`: a Starlark parser, per-rule matching, network rules,
//! prefix rules). That is the right shape for its breadth of tools. Here the
//! question is much smaller -- one Tool, already inside a container that blocks
//! writes outside the Workspace, all network, and the credential directories --
//! so what an approval still protects is narrow: the user's own files inside
//! the Workspace. A command that cannot write has nothing left to approve.
//!
//! The structural idea taken from Codex is that the decision is about
//! *effects*, not about a name being on a list (`core/src/safety.rs`, where
//! auto-approval follows from the action being constrained). The name list here
//! is a means to that end, not the end.
//!
//! ## How it stays honest
//!
//! No shell parser. A parser would have to be right about quoting, expansion,
//! precedence and word splitting, and being subtly wrong about any of those is
//! exactly how this kind of code grants what it meant to withhold. Instead the
//! grammar accepted is deliberately tiny, and anything outside it asks:
//!
//! - separators: `|`, `&&`, `||`, `;` -- every segment must pass on its own
//! - words: bare, or wholly inside single quotes (sh expands nothing there)
//! - every other shell character asks, including `> < & $ ` ( ) { } \ " ~ * ?`
//! - the executable must be a bare name on the list, never a path
//!
//! Rejecting globs (`*`, `?`) costs some convenience and removes a class of
//! reasoning about what a pattern expands to; it can be revisited with evidence.

/// The classifier's answer. Two values on purpose: a third ("probably fine")
/// would be a place for uncertainty to hide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellCommandClass {
    /// Cannot write anything, so an approval would protect nothing.
    ProvablyReadOnly,
    /// Everything else, including everything unparsed and everything unknown.
    RequiresApproval,
}

/// Executables with no way to write, by flag **or by positional argument**.
///
/// The earlier version of this list said "no write capability reachable through
/// any flag", and that wording is what broke it. A 2026-08-07 review found five
/// writable commands on it: `uniq in out` writes through a positional argument
/// rather than a flag, `file -C` compiles a magic database, `tree -o` writes its
/// output, `date` with an operand sets the system clock, and git's write modes
/// live in subcommands whose names read as queries.
///
/// Each entry below has been checked for both shapes. Anything that could not be
/// ruled out by inspecting the executable alone is absent, which is why `sort`,
/// `sed`, `tee`, `awk`, `find` and now `date`, `file`, `tree` and `uniq` are not
/// here.
const READ_ONLY_COMMANDS: &[&str] = &[
    "basename", "cat", "cksum", "dirname", "du", "echo", "grep", "head", "hostname", "ls", "nl",
    "pwd", "rg", "stat", "tail", "wc", "whoami", "which",
];

/// `git` used to have an exception here for "read-only subcommands". It was
/// wrong and has been withdrawn entirely.
///
/// The exception assumed a subcommand name determines its effects. It does not:
/// `branch -D` and `tag -d` delete, and the whole diff family -- `diff`, `log`,
/// `show` -- accepts `--output=<file>`, so the write capability is in the shared
/// option surface rather than in any one subcommand. Enumerating safe
/// subcommands cannot be right while that is true, and being wrong here is an
/// approval bypass rather than an inconvenience.
///
/// Left as an empty constant rather than deleted so the reason stays attached to
/// the decision.
const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[];

/// Characters that introduce an effect this classifier does not model.
const REJECTED_CHARACTERS: &[char] = &[
    '>', '<', '&', '$', '`', '(', ')', '{', '}', '\\', '"', '~', '*', '?', '[', ']', '!', '#',
    '\n', '\r', '\0',
];

pub fn classify_shell_command(command: &str) -> ShellCommandClass {
    match segments(command) {
        Some(segments) if !segments.is_empty() => {
            if segments.iter().all(|segment| is_read_only_segment(segment)) {
                ShellCommandClass::ProvablyReadOnly
            } else {
                ShellCommandClass::RequiresApproval
            }
        }
        _ => ShellCommandClass::RequiresApproval,
    }
}

/// Splits on `|`, `&&`, `||` and `;`, but only outside single quotes, so a
/// separator inside a quoted argument stays part of that argument. Returns
/// `None` when the command leaves a quote open, because then the split cannot
/// be trusted at all.
fn segments(command: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '\'' {
            quoted = !quoted;
            current.push(character);
            continue;
        }
        if quoted {
            current.push(character);
            continue;
        }
        match character {
            ';' => {
                segments.push(std::mem::take(&mut current));
            }
            '|' | '&' => {
                // `&&` and `||` are separators; a single `&` is backgrounding
                // and a single `|` is a pipe. Backgrounding is rejected later
                // by the character rule, so only the doubled forms are consumed
                // here, along with the single pipe.
                if characters.peek() == Some(&character) {
                    characters.next();
                    segments.push(std::mem::take(&mut current));
                } else if character == '|' {
                    segments.push(std::mem::take(&mut current));
                } else {
                    current.push(character);
                }
            }
            _ => current.push(character),
        }
    }
    if quoted {
        return None;
    }
    segments.push(current);
    let segments: Vec<String> = segments
        .into_iter()
        .map(|segment| segment.trim().to_owned())
        .filter(|segment| !segment.is_empty())
        .collect();
    Some(segments)
}

fn is_read_only_segment(segment: &str) -> bool {
    let Some(words) = words(segment) else {
        return false;
    };
    let Some((executable, arguments)) = words.split_first() else {
        return false;
    };
    // A path is not the name it looks like; only bare names are classified.
    if executable.contains('/') {
        return false;
    }
    // `NAME=value cmd` changes how the command resolves and runs.
    if executable.contains('=') {
        return false;
    }
    if executable == "git" {
        return arguments
            .first()
            .is_some_and(|subcommand| READ_ONLY_GIT_SUBCOMMANDS.contains(&subcommand.as_str()));
    }
    // An operand-count rule was tried here and removed the same day: it read
    // `head -n 20 file` as two operands, because `20` is an option's value and
    // telling those apart needs the per-command knowledge this was meant to
    // avoid. It was the original mistake in a new shape -- a general rule that
    // is only correct if you already know each command.
    //
    // What is left is a curated list, and a curated list is only as good as the
    // review behind it. That is a real weakness, not a solved problem, and it is
    // why this classifier is currently unused: see ADR-0039 Status.
    READ_ONLY_COMMANDS.contains(&executable.as_str())
}

/// Splits a segment into words. Bare words may not contain any rejected
/// character; single-quoted spans may contain anything but a quote, because sh
/// performs no expansion inside them. Returns `None` if anything is outside
/// that grammar.
fn words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;

    for character in segment.chars() {
        if character == '\'' {
            quoted = !quoted;
            started = true;
            continue;
        }
        if quoted {
            current.push(character);
            continue;
        }
        if character.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut current));
                started = false;
            }
            continue;
        }
        if REJECTED_CHARACTERS.contains(&character) {
            return None;
        }
        current.push(character);
        started = true;
    }
    if quoted {
        return None;
    }
    if started {
        words.push(current);
    }
    Some(words)
}
