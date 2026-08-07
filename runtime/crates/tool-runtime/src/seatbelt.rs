//! macOS Seatbelt containment for trusted native Tools.
//!
//! Design adapted from OpenAI Codex (Apache-2.0), `codex-rs/sandboxing`. Two of
//! its decisions are load-bearing and are reproduced deliberately:
//!
//! 1. `/usr/bin/sandbox-exec` is referenced by absolute path. Resolving it
//!    through `PATH` would let an attacker who can write a `PATH` entry replace
//!    the sandbox with a no-op.
//! 2. Reads stay broadly open with **targeted denials layered on top**. A read
//!    allowlist tight enough to be meaningful stops dyld from loading and the
//!    process aborts before it runs; that finding stands. What does work is the
//!    shape Codex actually uses: everything readable, then carve-outs. Codex
//!    reaches it two ways -- `require-not` exclusions under a `/` readable root,
//!    and standalone `(deny file-read* (regex …))` appended *after* the allow,
//!    relying on SBPL's last-match-wins. This module uses the latter with
//!    `subpath` parameters, since the protected paths here are fixed
//!    directories rather than globs.
//!
//! What this does and does not protect:
//!
//! | Property | Contained |
//! | --- | --- |
//! | Writing outside the Workspace | yes |
//! | Outbound network | yes |
//! | Reading credential directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`) | yes |
//! | Reading everything else outside the Workspace | **no** |
//!
//! The last row is a real limitation, not an oversight. Confidentiality is
//! protected only for the enumerated directories; a contained Tool still reads
//! any other file its user can read.
//!
//! Links, which a path-based denial is the natural thing to worry about:
//!
//! - **Symlinks do not bypass it.** The kernel applies the rule to the resolved
//!   path, so a link the Tool plants in its own Workspace is created fine and
//!   then cannot be followed.
//! - **Hard links cannot be created at all**, and *not* because of the denial:
//!   `(allow file-write* …)` does not grant `file-link`. Measured by removing
//!   the denial, where the link is still refused -- at the destination instead
//!   of the source. Do not read the hard-link test as covering the denial.

use crate::WorkspaceAccess;
use std::path::{Path, PathBuf};

/// Absolute path so a hostile `PATH` cannot substitute a permissive stand-in.
pub(crate) const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// Parameter name the profile reads the Workspace root from. Paths are passed
/// as `-D` parameters rather than interpolated into the profile text, so a
/// Workspace path containing profile syntax cannot rewrite the policy.
pub(crate) const WORKSPACE_PARAM: &str = "AGENT_RUNTIME_WORKSPACE";

/// Denied read paths are numbered parameters for the same reason the Workspace
/// is one: a directory name containing profile syntax must not be able to
/// rewrite the policy that contains it.
pub(crate) const DENIED_READ_PARAM_PREFIX: &str = "AGENT_RUNTIME_DENIED_READ_";

const BASE_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow mach-lookup)
; Reads stay open here; targeted (deny file-read* …) rules are appended after
; this, and SBPL resolves by last match. A read *allowlist* instead of this
; allow-then-deny shape prevents dyld from loading at all.
(allow file-read*)
(allow file-write-data
  (literal "/dev/null")
  (literal "/dev/stdout")
  (literal "/dev/stderr"))
"#;

const WRITABLE_WORKSPACE: &str = r#"(allow file-write* (subpath (param "AGENT_RUNTIME_WORKSPACE")))
"#;

/// Builds the profile for one Tool. No network rule is ever emitted, so
/// `(deny default)` keeps every socket operation closed.
///
/// `denied_read_count` deny rules are appended *after* the blanket
/// `(allow file-read*)`; SBPL resolves by last match, so ordering is what makes
/// the denial effective.
pub(crate) fn profile_for(access: WorkspaceAccess, denied_read_count: usize) -> String {
    let mut profile = String::from(BASE_PROFILE);
    if access == WorkspaceAccess::ReadWrite {
        profile.push_str(WRITABLE_WORKSPACE);
    }
    for index in 0..denied_read_count {
        let param = format!("{DENIED_READ_PARAM_PREFIX}{index}");
        // `subpath` covers the contents; `literal` covers the directory node
        // itself, which `subpath` alone leaves reachable.
        profile.push_str(&format!(
            "(deny file-read* (subpath (param \"{param}\")) (literal (param \"{param}\")))\n"
        ));
        // Writes too: a denied directory that happens to sit inside a writable
        // Workspace would otherwise stay probeable through create and unlink.
        profile.push_str(&format!(
            "(deny file-write* (subpath (param \"{param}\")) (literal (param \"{param}\")))\n"
        ));
    }
    profile
}

/// Resolves the home directory containment is built from. `std::env::home_dir`
/// reads `$HOME` and falls back to the passwd database on Unix, so an
/// environment that simply forgot to export `HOME` still gets contained.
pub(crate) fn containment_home() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir().filter(|home| home.is_absolute())
}

/// Fail closed. Without a home directory the credential denials cannot be built,
/// and launching anyway is the silent degradation this exists to prevent: the
/// Tool would run with a profile that reads as contained and is not.
pub(crate) fn required_read_denials(
    home: Option<&Path>,
) -> Result<Vec<PathBuf>, ContainmentUnavailable> {
    home.map(sensitive_read_denials)
        .ok_or(ContainmentUnavailable)
}

/// The home directory could not be resolved, so credential containment cannot be
/// established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContainmentUnavailable;

/// Credential directories a contained Tool must not read. Taking `home` as an
/// argument rather than reading `$HOME` keeps this a pure function, so tests
/// exercise it against a temporary directory instead of the real one.
pub(crate) fn sensitive_read_denials(home: &Path) -> Vec<PathBuf> {
    [".ssh", ".aws", ".gnupg", ".config/gh"]
        .iter()
        .map(|suffix| normalize_for_sandbox(&home.join(suffix)))
        .collect()
}

/// Seatbelt evaluates the path the kernel resolves, not the one we wrote. On
/// macOS `/var` and `/tmp` are symlinks into `/private`, so an unresolved prefix
/// produces a rule that silently never matches -- worse than no rule, because it
/// reads as protection. Codex normalizes for the same reason
/// (`codex-rs/sandboxing/src/seatbelt.rs`, `normalize_path_for_sandbox`).
///
/// The path may not exist yet, so this resolves the longest existing ancestor
/// and re-attaches the remainder rather than requiring the whole path.
fn normalize_for_sandbox(path: &Path) -> PathBuf {
    let mut suffix = Vec::new();
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(resolved) = probe.canonicalize() {
            let mut normalized = resolved;
            for component in suffix.iter().rev() {
                normalized.push(component);
            }
            return normalized;
        }
        let Some(name) = probe.file_name().map(std::ffi::OsString::from) else {
            return path.to_path_buf();
        };
        suffix.push(name);
        if !probe.pop() {
            return path.to_path_buf();
        }
    }
}

/// Wraps a launch as `sandbox-exec -p <profile> -D KEY=VALUE -- program args…`.
pub(crate) fn wrap_launch(
    program: &Path,
    args: &[String],
    workspace: &Path,
    access: WorkspaceAccess,
    denied_reads: &[PathBuf],
) -> (String, Vec<String>) {
    // Normalized here rather than trusting the caller: an unresolved prefix
    // makes the Workspace rule silently inert, exactly as it does for the denial
    // rules, and nothing in the signature communicated that obligation.
    let workspace = normalize_for_sandbox(workspace);
    let mut wrapped = vec![
        "-p".to_string(),
        profile_for(access, denied_reads.len()),
        "-D".to_string(),
        format!("{WORKSPACE_PARAM}={}", workspace.display()),
    ];
    for (index, denied) in denied_reads.iter().enumerate() {
        wrapped.push("-D".to_string());
        wrapped.push(format!(
            "{DENIED_READ_PARAM_PREFIX}{index}={}",
            denied.display()
        ));
    }
    wrapped.push("--".to_string());
    wrapped.push(program.display().to_string());
    wrapped.extend(args.iter().cloned());
    (SEATBELT_EXECUTABLE.to_string(), wrapped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("agent-seatbelt-{label}-{}", uuid::Uuid::now_v7()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn probe_script(root: &Path, body: &str) -> std::path::PathBuf {
        let executable = root.join("probe");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }

    /// Runs the probe inside the real Seatbelt container.
    fn run_contained(
        probe: &Path,
        workspace: &Path,
        denied: &[std::path::PathBuf],
    ) -> std::process::Output {
        run_contained_with(probe, workspace, denied, WorkspaceAccess::ReadOnly)
    }

    /// Link probes need to *write* into the Workspace to plant a link, which is
    /// a capability a ReadWrite Tool genuinely has.
    fn run_contained_with(
        probe: &Path,
        workspace: &Path,
        denied: &[std::path::PathBuf],
        access: WorkspaceAccess,
    ) -> std::process::Output {
        let (program, args) = wrap_launch(probe, &[], workspace, access, denied);
        Command::new(program).args(args).output().unwrap()
    }

    /// The defect this closes: a contained Tool could read anything its user
    /// could, including credential directories. Temporary directories only --
    /// nothing here touches the real `~/.ssh`, `~/.aws`, `~/.config/gh` or
    /// `~/.gnupg`.
    #[test]
    fn a_contained_tool_cannot_read_a_protected_directory() {
        let root = temporary_directory("deny-read");
        let workspace = temporary_directory("deny-read-ws");
        let home = temporary_directory("deny-read-home");
        let secrets = home.join(".ssh");
        fs::create_dir_all(&secrets).unwrap();
        let secret = secrets.join("id_ed25519");
        fs::write(&secret, "not-a-real-key\n").unwrap();

        let probe = probe_script(&root, &format!("exec /bin/cat '{}'", secret.display()));
        let output = run_contained(&probe, &workspace, &sensitive_read_denials(&home));

        assert!(
            !output.status.success(),
            "a contained Tool read a protected path; stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("not-a-real-key"),
            "protected file contents reached the contained Tool"
        );
    }

    /// The denial must be targeted. A read allowlist tight enough to be
    /// meaningful stops dyld from loading and the process aborts before it runs,
    /// so the failure mode to guard against is "nothing runs at all" passing as
    /// containment: without this control, the test above would pass for the
    /// wrong reason.
    #[test]
    fn ordinary_reads_still_work_alongside_the_denial() {
        let root = temporary_directory("deny-read-control");
        let workspace = temporary_directory("deny-read-control-ws");
        let home = temporary_directory("deny-read-control-home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let ordinary = workspace.join("readable.txt");
        fs::write(&ordinary, "ordinary-content\n").unwrap();

        let probe = probe_script(&root, &format!("exec /bin/cat '{}'", ordinary.display()));
        let output = run_contained(&probe, &workspace, &sensitive_read_denials(&home));

        assert!(
            output.status.success(),
            "the denial broke ordinary reads; stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("ordinary-content"));
    }

    /// A protected directory must not be probeable by listing it either.
    #[test]
    fn a_contained_tool_cannot_enumerate_a_protected_directory() {
        let root = temporary_directory("deny-list");
        let workspace = temporary_directory("deny-list-ws");
        let home = temporary_directory("deny-list-home");
        let secrets = home.join(".aws");
        fs::create_dir_all(&secrets).unwrap();
        fs::write(secrets.join("credentials"), "[default]\n").unwrap();

        let probe = probe_script(&root, &format!("exec /bin/ls '{}'", secrets.display()));
        let output = run_contained(&probe, &workspace, &sensitive_read_denials(&home));

        assert!(
            !output.status.success(),
            "a contained Tool enumerated a protected directory; stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("credentials"));
    }

    /// An unresolvable home must refuse, not degrade. Returning an empty denial
    /// list here would produce a profile that reads as contained and is not --
    /// the exact failure ADR-0037 recorded as its own gap.
    #[test]
    fn an_unresolvable_home_refuses_instead_of_producing_an_empty_denial_set() {
        assert_eq!(required_read_denials(None), Err(ContainmentUnavailable));
    }

    #[test]
    fn a_resolvable_home_yields_the_full_denial_set() {
        let home = Path::new("/Users/example");
        let denials = required_read_denials(Some(home)).expect("a resolvable home must succeed");
        assert_eq!(denials.len(), 4);
    }

    /// `$HOME` is the usual source, but a Worker started without it must still be
    /// contained rather than silently unprotected.
    #[test]
    fn the_home_directory_resolves_to_an_absolute_path() {
        let home = containment_home().expect("this host has a resolvable home directory");
        assert!(home.is_absolute(), "containment home is relative: {home:?}");
    }

    /// Builds a home with a protected `.ssh/id_ed25519` and returns
    /// `(home, secret_path)`.
    fn home_with_secret(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let home = temporary_directory(label);
        let secrets = home.join(".ssh");
        fs::create_dir_all(&secrets).unwrap();
        let secret = secrets.join("id_ed25519");
        fs::write(&secret, "not-a-real-key\n").unwrap();
        (home, secret)
    }

    const SECRET: &str = "not-a-real-key";
    /// Every link probe ends by printing this. Asserting the secret is absent is
    /// satisfied by a probe that never ran at all, so absence alone proves
    /// nothing; the marker is what makes the assertion mean something.
    const RAN: &str = "PROBE_RAN";

    /// The Tool is the untrusted party and it may write inside its Workspace, so
    /// planting a symlink and following it is entirely within the powers it was
    /// granted. Measured: creating the link succeeds, following it does not --
    /// the kernel applies the denial to the resolved path.
    #[test]
    fn a_symlink_the_tool_plants_itself_does_not_reach_a_protected_path() {
        let root = temporary_directory("link-self");
        let workspace = temporary_directory("link-self-ws");
        let (home, secret) = home_with_secret("link-self-home");

        let probe = probe_script(
            &root,
            &format!(
                "/bin/ln -s '{secret}' '{ws}/link' && echo LINK_CREATED\n\
                 /bin/cat '{ws}/link' 2>/dev/null || echo FOLLOW_DENIED\n\
                 echo {RAN}",
                secret = secret.display(),
                ws = workspace.display()
            ),
        );
        let output = run_contained_with(
            &probe,
            &workspace,
            &sensitive_read_denials(&home),
            WorkspaceAccess::ReadWrite,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(stdout.contains(RAN), "the probe did not run: {stdout}");
        assert!(
            stdout.contains("LINK_CREATED"),
            "the Tool could not even plant the link, so this proves nothing about \
             following one: {stdout}"
        );
        assert!(
            stdout.contains("FOLLOW_DENIED") && !stdout.contains(SECRET),
            "a symlink planted inside the Workspace reached a protected path: {stdout}"
        );
    }

    /// A hard link is a second directory entry for the same inode at a different
    /// path, so a path-based rule is exactly the kind a hard link can slip past.
    /// Measured, and **not** by the credential denial: with the denial removed
    /// the link is still refused, at the destination rather than the source.
    /// `(allow file-write* …)` does not grant `file-link`, so a contained Tool
    /// cannot create a hard link anywhere, protected target or not.
    ///
    /// The name says what is actually pinned. This test does not detect a
    /// regression in the credential denial -- it detects someone granting
    /// `file-link` or broadening the write rule, which is what would open the
    /// hard-link route in the first place.
    #[test]
    fn a_contained_tool_cannot_create_a_hard_link_at_all() {
        let root = temporary_directory("hardlink-self");
        let workspace = temporary_directory("hardlink-self-ws");
        let (home, secret) = home_with_secret("hardlink-self-home");

        let probe = probe_script(
            &root,
            &format!(
                "/bin/ln '{secret}' '{ws}/hard' 2>/dev/null && echo HARD_CREATED\n\
                 /bin/cat '{ws}/hard' 2>/dev/null || echo HARD_UNREADABLE\n\
                 echo {RAN}",
                secret = secret.display(),
                ws = workspace.display()
            ),
        );
        // ReadWrite on purpose: the Workspace must genuinely be writable, so a
        // refused link is about linking rather than about an unwritable
        // destination.
        let output = run_contained_with(
            &probe,
            &workspace,
            &sensitive_read_denials(&home),
            WorkspaceAccess::ReadWrite,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(stdout.contains(RAN), "the probe did not run: {stdout}");
        assert!(
            !stdout.contains("HARD_CREATED"),
            "a hard link was created inside the container, opening a path-based \
             denial's blind spot: {stdout}"
        );
        assert!(
            !stdout.contains(SECRET),
            "a hard link reached a protected path: {stdout}"
        );
    }

    /// A Workspace can already contain a symlink when the Tool starts -- planted
    /// by an earlier Run, restored from a Checkpoint, or simply part of the
    /// user's tree. The Tool then needs no write capability at all.
    #[test]
    fn a_symlink_already_present_in_the_workspace_does_not_reach_a_protected_path() {
        let root = temporary_directory("link-pre");
        let workspace = temporary_directory("link-pre-ws");
        let (home, secret) = home_with_secret("link-pre-home");
        std::os::unix::fs::symlink(&secret, workspace.join("planted")).unwrap();

        let probe = probe_script(
            &root,
            &format!(
                "/bin/cat '{ws}/planted' 2>/dev/null || echo FOLLOW_DENIED\necho {RAN}",
                ws = workspace.display()
            ),
        );
        let output = run_contained(&probe, &workspace, &sensitive_read_denials(&home));
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(stdout.contains(RAN), "the probe did not run: {stdout}");
        assert!(
            stdout.contains("FOLLOW_DENIED") && !stdout.contains(SECRET),
            "a pre-existing Workspace symlink reached a protected path: {stdout}"
        );
    }

    /// The control for all three: following a symlink to an ordinary file must
    /// still work. Without this, "the link could not be followed" would pass
    /// even if link resolution were broken outright, which would make the three
    /// tests above green for the wrong reason.
    #[test]
    fn following_a_symlink_to_an_ordinary_file_still_works() {
        let root = temporary_directory("link-control");
        let workspace = temporary_directory("link-control-ws");
        let (home, _secret) = home_with_secret("link-control-home");
        fs::write(workspace.join("plain.txt"), "ordinary-content\n").unwrap();

        let probe = probe_script(
            &root,
            &format!(
                "/bin/ln -s '{ws}/plain.txt' '{ws}/ok' && /bin/cat '{ws}/ok'\necho {RAN}",
                ws = workspace.display()
            ),
        );
        let output = run_contained_with(
            &probe,
            &workspace,
            &sensitive_read_denials(&home),
            WorkspaceAccess::ReadWrite,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(stdout.contains(RAN), "the probe did not run: {stdout}");
        assert!(
            stdout.contains("ordinary-content"),
            "symlink resolution is broken outright, so the denial tests prove \
             nothing: {stdout}"
        );
    }

    #[test]
    fn the_denied_set_covers_the_credential_directories_this_platform_protects() {
        let home = Path::new("/Users/example");
        let denied = sensitive_read_denials(home);

        for expected in [".ssh", ".aws", ".gnupg", ".config/gh"] {
            assert!(
                denied.contains(&home.join(expected)),
                "{expected} is not protected; denied = {denied:?}"
            );
        }
    }

    /// Denied paths reach the profile as `-D` parameters, never interpolated
    /// into the policy text, so a directory name containing profile syntax
    /// cannot rewrite the rule meant to contain it.
    #[test]
    fn denied_paths_are_passed_as_parameters_and_never_interpolated() {
        let hostile = std::path::PathBuf::from("/tmp/x\") (allow file-read*) (subpath \"/");
        let (program, args) = wrap_launch(
            Path::new("/usr/bin/true"),
            &[],
            Path::new("/tmp"),
            WorkspaceAccess::ReadOnly,
            &[hostile],
        );

        assert_eq!(program, SEATBELT_EXECUTABLE);
        let profile = &args[1];
        assert!(
            !profile.contains("(allow file-read*) (subpath"),
            "a denied path leaked into the profile text: {profile}"
        );
        assert!(profile.contains("(deny file-read*"));
        assert_eq!(args.iter().filter(|arg| *arg == "--").count(), 1);
    }

    #[test]
    fn a_read_only_profile_grants_no_workspace_write_and_no_network() {
        let profile = profile_for(WorkspaceAccess::ReadOnly, 0);
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(!profile.contains("file-write* (subpath"));
        assert!(!profile.contains("network"));
    }

    #[test]
    fn a_read_write_profile_grants_writes_only_through_the_workspace_parameter() {
        let profile = profile_for(WorkspaceAccess::ReadWrite, 0);
        assert!(
            profile.contains(r#"(allow file-write* (subpath (param "AGENT_RUNTIME_WORKSPACE")))"#)
        );
        assert!(!profile.contains("network"));
    }

    #[test]
    fn the_workspace_path_is_passed_as_a_parameter_and_never_interpolated() {
        let (program, args) = wrap_launch(
            Path::new("/usr/bin/true"),
            &["--stdio".to_string()],
            // A path that would rewrite the policy if it were interpolated.
            Path::new("/tmp/ws\") (allow network-outbound) (subpath \"/"),
            WorkspaceAccess::ReadWrite,
            &[],
        );
        assert_eq!(program, SEATBELT_EXECUTABLE);
        let profile = &args[1];
        assert!(
            !profile.contains("network-outbound"),
            "workspace path leaked into the profile text: {profile}"
        );
        assert!(args.iter().any(|arg| arg.starts_with(WORKSPACE_PARAM)));
        assert_eq!(args.iter().filter(|arg| *arg == "--").count(), 1);
    }
}
