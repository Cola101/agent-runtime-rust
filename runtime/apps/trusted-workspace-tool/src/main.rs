use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const MAX_REQUEST_BYTES: u64 = 128 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Per stream, so a command cannot exhaust Worker memory through its result.
const MAX_STREAM_BYTES: usize = 64 * 1024;
/// Absolute, so a hostile PATH entry cannot substitute the shell.
const SHELL: &str = "/bin/sh";
/// Fixed and system-only. The Workspace is deliberately absent, so a binary the
/// model drops beside its command is not reachable by bare name.
const FIXED_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// `$HOME` for commands: inside the Workspace, but not the Workspace root.
const AGENT_HOME_DIRECTORY: &str = ".agent-home";

#[derive(Debug, Deserialize)]
struct ToolProcessRequest {
    schema_version: u32,
    tool_call: ToolCall,
    binding_digest: String,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    id: String,
    name: String,
    /// Kept untyped so each operation parses only what it needs; the operations
    /// no longer share a shape now that `shell.exec` takes a command rather than
    /// a path.
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct PathArguments {
    path: String,
    /// Present only for `workspace.write_text`.
    #[serde(default)]
    text: Option<String>,
    /// What the caller believes the file holds right now, when it believes
    /// anything. The executor records this from the read that produced the
    /// contents being written; absent when the Run never read the file, which
    /// is an ordinary create or a deliberate replacement rather than a
    /// blind one.
    ///
    /// Checked here rather than by the caller because containment is checked
    /// here: the caller does not resolve paths inside the workspace and must
    /// not start, so it also cannot be the one to read what is there.
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShellArguments {
    command: String,
}

#[derive(Debug, Serialize)]
struct ToolProcessResult {
    tool_call_id: String,
    binding_digest: String,
    content: Value,
    is_error: bool,
}

fn main() -> ExitCode {
    if std::env::args_os().collect::<Vec<_>>().as_slice()
        != [
            std::ffi::OsString::from("agent-trusted-workspace-tool"),
            std::ffi::OsString::from("--stdio"),
        ]
        && std::env::args_os().skip(1).collect::<Vec<_>>() != ["--stdio"]
    {
        eprintln!("trusted workspace tool requires --stdio");
        return ExitCode::from(2);
    }
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("trusted workspace tool rejected its request: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), &'static str> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut input)
        .map_err(|_| "stdin_read_failed")?;
    if input.len() as u64 > MAX_REQUEST_BYTES {
        return Err("request_too_large");
    }
    let request: ToolProcessRequest = serde_json::from_slice(&input).map_err(|_| "invalid_json")?;
    validate_request(&request)?;

    let content = if request.tool_call.name == "shell.exec" {
        let arguments: ShellArguments = serde_json::from_value(request.tool_call.arguments.clone())
            .map_err(|_| "invalid_arguments")?;
        match shell_exec(&arguments.command) {
            Ok(content) => content,
            Err(code) => json!({"error":{"code":code}}),
        }
    } else {
        let arguments: PathArguments = serde_json::from_value(request.tool_call.arguments.clone())
            .map_err(|_| "invalid_arguments")?;
        let outcome = if request.tool_call.name == "workspace.write_text" {
            let text = arguments.text.as_deref().ok_or("missing_text")?;
            write_text(&arguments.path, text, arguments.expected_sha256.as_deref())
        } else {
            read_text(&arguments.path)
        };
        match outcome {
            Ok((path, text, bytes)) => json!({"path": path, "text": text, "bytes": bytes}),
            Err(code) => json!({"error":{"code":code}}),
        }
    };
    let result = ToolProcessResult {
        tool_call_id: request.tool_call.id,
        binding_digest: request.binding_digest,
        is_error: content.get("error").is_some(),
        content,
    };
    serde_json::to_writer(std::io::stdout(), &result).map_err(|_| "stdout_write_failed")?;
    std::io::stdout().flush().map_err(|_| "stdout_flush_failed")
}

fn validate_request(request: &ToolProcessRequest) -> Result<(), &'static str> {
    if request.schema_version != 1 {
        return Err("unsupported_schema");
    }
    if request.tool_call.id.trim().is_empty() || request.tool_call.id.len() > 256 {
        return Err("invalid_tool_call_id");
    }
    if !matches!(
        request.tool_call.name.as_str(),
        "workspace.read_text" | "workspace.write_text" | "shell.exec"
    ) {
        return Err("unsupported_tool");
    }
    if request.binding_digest.len() != 64
        || !request
            .binding_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("invalid_binding_digest");
    }
    Ok(())
}

/// Runs a model-authored command.
///
/// The command is passed to `/bin/sh -c` as a single argument. ADR-0025's
/// "no shell, no argument concatenation" rule does not transfer here: it exists
/// so a crafted argument cannot alter a command *we* composed, and there is no
/// such command -- the model authors the whole thing by design. What keeps this
/// bounded is the container around it, the environment below, and the approval
/// in front of it.
///
/// A non-zero exit is a result, not a Tool failure. Only the Tool being unable
/// to run the command at all is an error, because that is the case where the
/// side effects are unknown and fail-closed has to apply.
fn shell_exec(command: &str) -> Result<Value, &'static str> {
    if command.trim().is_empty() {
        return Err("empty_command");
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err("command_too_large");
    }
    if command.contains('\0') {
        return Err("command_contains_nul");
    }

    // Contained, and out of sight: HOME at the Workspace root makes macOS
    // frameworks create Library/Caches there, so the model's own directory
    // fills with junk. Measured, not assumed.
    let home = std::env::current_dir()
        .map_err(|_| "workspace_unavailable")?
        .join(AGENT_HOME_DIRECTORY);
    fs::create_dir_all(&home).map_err(|_| "agent_home_unavailable")?;

    let output = Command::new(SHELL)
        .arg("-c")
        .arg(command)
        // Nothing is inherited. The Worker's environment holds provider
        // credentials, database passwords and NATS credentials, and a
        // model-authored command must not see any of it. What the command does
        // get is fixed here, absolute, and does not include the Workspace, so a
        // binary dropped next to the command is not on PATH.
        .env_clear()
        .env("PATH", FIXED_PATH)
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| "command_spawn_failed")?;

    let (stdout, stdout_truncated) = bounded(&output.stdout);
    let (stderr, stderr_truncated) = bounded(&output.stderr);
    Ok(json!({
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stdout_truncated": stdout_truncated,
        "stderr": stderr,
        "stderr_truncated": stderr_truncated,
    }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

/// Truncates on a character boundary so the result is always valid UTF-8 and
/// always bounded, whatever the command emitted.
fn bounded(raw: &[u8]) -> (String, bool) {
    let truncated = raw.len() > MAX_STREAM_BYTES;
    let mut end = raw.len().min(MAX_STREAM_BYTES);
    while end > 0 && std::str::from_utf8(&raw[..end]).is_err() {
        end -= 1;
    }
    (String::from_utf8_lossy(&raw[..end]).into_owned(), truncated)
}

/// Resolves a caller-supplied relative path against the Workspace, refusing
/// anything that could leave it. Seatbelt is the outer boundary; this is the
/// inner one, so a containment gap alone is not enough to escape.
fn resolve_within_workspace(requested: &str) -> Result<PathBuf, &'static str> {
    if requested.is_empty() || requested.len() > 1024 {
        return Err("path_not_allowed");
    }
    let relative = Path::new(requested);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("path_not_allowed");
    }
    let mut candidate = PathBuf::from(".");
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err("path_not_allowed");
        };
        candidate.push(segment);
        // A symlink anywhere on the path could redirect the write, so every
        // existing ancestor is checked, not just the final component.
        if let Ok(metadata) = fs::symlink_metadata(&candidate)
            && metadata.file_type().is_symlink()
        {
            return Err("path_not_allowed");
        }
    }
    Ok(candidate)
}

fn write_text(
    requested: &str,
    text: &str,
    expected: Option<&str>,
) -> Result<(String, String, usize), &'static str> {
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err("file_too_large");
    }
    let candidate = resolve_within_workspace(requested)?;
    // Before anything is opened for writing. A write that would replace
    // something other than what the caller read is refused whole: the point is
    // that the edit which arrived in between survives, so nothing may be
    // truncated first.
    if let Some(expected) = expected {
        let held = fs::read(&candidate).map_err(|_| "file_changed_since_read")?;
        if sha256_hex(&held) != expected {
            return Err("file_changed_since_read");
        }
    }
    // Refuse to replace anything that is not already a regular file, so a write
    // cannot clobber a directory or a device node.
    if let Ok(metadata) = fs::symlink_metadata(&candidate)
        && !metadata.file_type().is_file()
    {
        return Err("path_not_allowed");
    }
    let parent = candidate.parent().ok_or("path_not_allowed")?;
    if !parent.as_os_str().is_empty() && !parent.is_dir() {
        return Err("path_not_allowed");
    }
    fs::write(&candidate, text.as_bytes()).map_err(|_| "write_failed")?;
    Ok((requested.to_owned(), text.to_owned(), text.len()))
}

fn read_text(requested: &str) -> Result<(String, String, usize), &'static str> {
    if requested.is_empty() || requested.len() > 1024 {
        return Err("path_not_allowed");
    }
    let relative = Path::new(requested);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("path_not_allowed");
    }

    let mut candidate = PathBuf::from(".");
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err("path_not_allowed");
        };
        candidate.push(segment);
        let metadata = fs::symlink_metadata(&candidate).map_err(|_| "path_not_allowed")?;
        if metadata.file_type().is_symlink() {
            return Err("path_not_allowed");
        }
    }
    let metadata = fs::metadata(&candidate).map_err(|_| "path_not_allowed")?;
    if !metadata.is_file() {
        return Err("path_not_allowed");
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("file_too_large");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(&candidate)
        .map_err(|_| "path_not_allowed")?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "read_failed")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("file_too_large");
    }
    let text = String::from_utf8(bytes).map_err(|_| "not_utf8")?;
    let byte_count = text.len();
    Ok((requested.to_owned(), text, byte_count))
}
