use agent_edge_node::daemon::EdgeDaemon;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

fn parse_config_path<I, S>(arguments: I) -> Result<PathBuf, &'static str>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    if arguments.next().as_ref().map(AsRef::as_ref) != Some("--config") {
        return Err("usage: agent-edge-node --config <absolute-path>");
    }
    let path = arguments
        .next()
        .ok_or("usage: agent-edge-node --config <absolute-path>")?;
    if arguments.next().is_some() {
        return Err("usage: agent-edge-node --config <absolute-path>");
    }
    let path = PathBuf::from(path.as_ref());
    if !path.is_absolute() {
        return Err("Edge Node configuration path must be absolute");
    }
    Ok(path)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().init();
    let config_path = match parse_config_path(std::env::args()) {
        Ok(path) => path,
        Err(message) => {
            error!(
                component = "edge-node",
                error = message,
                "configuration rejected"
            );
            std::process::exit(2);
        }
    };
    let daemon = match EdgeDaemon::from_config_file(
        &config_path,
        chrono::Utc::now().timestamp_millis(),
    ) {
        Ok(daemon) => daemon,
        Err(error_value) => {
            error!(component = "edge-node", error = %error_value, "Edge Node initialization failed");
            std::process::exit(1);
        }
    };
    info!(
        component = "edge-node",
        node_id = %daemon.node_id(),
        node_generation = daemon.node_generation(),
        profiles = daemon.profile_count(),
        "Edge Node outbound Runtime started"
    );
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    if let Err(error_value) = daemon.run(shutdown).await {
        error!(component = "edge-node", error = %error_value, "Edge Node stopped with an error");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::parse_config_path;

    #[test]
    fn config_path_requires_one_explicit_flag_and_value() {
        assert!(parse_config_path(["edge-node", "--config", "/tmp/edge.json"]).is_ok());
        assert!(parse_config_path(["edge-node"]).is_err());
        assert!(parse_config_path(["edge-node", "/tmp/edge.json"]).is_err());
        assert!(
            parse_config_path(["edge-node", "--config", "/tmp/a", "--config", "/tmp/b"]).is_err()
        );
    }
}
