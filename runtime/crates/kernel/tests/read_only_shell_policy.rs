//! Which shell commands may run without asking a human.
//!
//! Every `shell.exec` call currently needs an approval, which makes the Tool
//! unusable for continuous work: a person clicks ten times and turns it off.
//! The fix is not "trust the model" but "decide, conservatively, which commands
//! cannot do anything an approval would have prevented".
//!
//! The container already blocks writes outside the Workspace, all network, and
//! the credential directories (ADR-0036, ADR-0037). What an approval still
//! guards is therefore narrow: destroying the user's own files inside the
//! Workspace. A command that provably cannot write anything has nothing left
//! for the human to approve.
//!
//! "Provably" is doing the work. This classifier answers only two ways --
//! read-only, or ask -- and every case it is not certain about is `ask`. The
//! tests below are mostly about the cases where a lazier classifier would say
//! read-only and be wrong.

use agent_kernel::{ShellCommandClass, classify_shell_command};

fn read_only(command: &str) -> bool {
    matches!(
        classify_shell_command(command),
        ShellCommandClass::ProvablyReadOnly
    )
}

#[test]
fn plain_read_only_commands_need_no_approval() {
    for command in [
        "ls",
        "ls -la",
        "cat README.md",
        "head -n 20 src/main.rs",
        "wc -l Cargo.toml",
        "pwd",
        "grep needle haystack.txt",
        "grep -rn needle src",
    ] {
        assert!(read_only(command), "should not need approval: {command}");
    }
}

#[test]
fn pipelines_and_and_chains_of_read_only_commands_stay_read_only() {
    for command in [
        "ls | wc -l",
        "cat a.txt | grep needle | head -n 3",
        "ls && pwd",
        "ls ; pwd",
    ] {
        assert!(read_only(command), "should not need approval: {command}");
    }
}

/// The whole command is one string, so a classifier that looked only at the
/// first word would approve this and then run `rm -rf`.
#[test]
fn a_chain_is_only_read_only_if_every_part_is() {
    for command in [
        "ls; rm -rf /",
        "ls && rm important.txt",
        "ls || rm important.txt",
        "cat a.txt | tee b.txt",
        "pwd; curl https://example.com",
    ] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// Redirection writes. It is the cheapest way to turn a read into a write and
/// the first thing a name-based allow-list misses.
#[test]
fn redirection_always_asks() {
    for command in [
        "cat a.txt > b.txt",
        "ls >> log",
        "ls > /dev/null",
        "cat < a.txt",
        "ls 2>&1",
    ] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// Substitution runs a command we never classified.
#[test]
fn command_substitution_always_asks() {
    for command in [
        "cat $(find . -name secret)",
        "echo `rm -rf /`",
        "ls ${HOME}",
        "cat file$(whoami)",
    ] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

#[test]
fn backgrounding_and_subshells_always_ask() {
    for command in ["ls &", "(ls)", "{ ls; }", "ls & pwd"] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// `git` is worth allowing -- status and log are most of what an Agent reads --
/// but only for subcommands that cannot write, and only when nothing precedes
/// the subcommand. `git -c` can define an alias that runs an arbitrary shell
/// command, which is a write capability wearing a read's name.
#[test]
fn git_is_allowed_only_for_read_only_subcommands_with_no_preceding_options() {
    for command in [
        "git status",
        "git log --oneline -5",
        "git diff",
        "git show HEAD",
        "git rev-parse HEAD",
        "git ls-files",
    ] {
        assert!(read_only(command), "should not need approval: {command}");
    }
    for command in [
        "git push",
        "git commit -m x",
        "git checkout main",
        "git clean -fd",
        "git reset --hard",
        "git -c alias.st=!rm status",
        "git -C /other status",
        "git --exec-path=/tmp status",
        "git",
    ] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// Single quotes suppress every expansion in sh, so a quoted argument is inert.
/// Double quotes do not, and getting their rules exactly right is where this
/// kind of code grows bugs, so they ask.
#[test]
fn single_quoted_arguments_are_allowed_and_double_quoted_ones_ask() {
    assert!(read_only("grep 'needle with spaces' file.txt"));
    assert!(read_only("grep 'a;b' file.txt"));
    assert!(!read_only("grep \"needle\" file.txt"));
    assert!(!read_only("grep 'unterminated file.txt"));
}

/// A quoted separator is data, not a separator. Splitting before accounting for
/// quotes would see a second command here that does not exist -- and the
/// reverse mistake, splitting after, would miss a real one.
#[test]
fn separators_inside_single_quotes_are_not_separators() {
    assert!(read_only("grep 'ls | rm -rf /' file.txt"));
}

#[test]
fn unknown_or_absent_commands_ask() {
    for command in [
        "",
        "   ",
        "rm -rf /",
        "curl https://example.com",
        "python3 -c pass",
        "LS",
    ] {
        assert!(!read_only(command), "must ask: {command:?}");
    }
}

/// An allow-list keyed on a bare name says nothing about a path, which may not
/// be the binary the name refers to.
#[test]
fn paths_rather_than_bare_names_ask() {
    for command in ["/bin/ls", "./ls", "../bin/cat file", "bin/ls"] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// Home expansion reaches outside the Workspace. The container stops the
/// credential directories specifically, not everything a user can read, so this
/// stays a decision for a person.
#[test]
fn home_and_glob_expansion_outside_the_workspace_asks() {
    for command in ["cat ~/.bashrc", "ls ~", "cat ~root/x"] {
        assert!(!read_only(command), "must ask: {command}");
    }
}

/// Environment assignment changes how the following command resolves and runs.
#[test]
fn leading_environment_assignment_asks() {
    for command in ["PATH=/tmp ls", "LD_PRELOAD=/tmp/x cat f"] {
        assert!(!read_only(command), "must ask: {command}");
    }
}
