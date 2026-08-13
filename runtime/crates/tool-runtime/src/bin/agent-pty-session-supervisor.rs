use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--state-root") {
        return Err("usage: agent-pty-session-supervisor --state-root <absolute-path>".into());
    }
    let state_root = PathBuf::from(args.next().ok_or("missing PTY supervisor state root")?);
    if args.next().is_some() {
        return Err("unexpected PTY supervisor argument".into());
    }
    agent_tool_runtime::run_process_session_pty_supervisor(state_root).await?;
    Ok(())
}
