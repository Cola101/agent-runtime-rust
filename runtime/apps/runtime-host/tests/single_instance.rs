//! One state root, one daemon.
//!
//! `bind` used to delete any socket it found before binding, on the theory that
//! a leftover socket must be from a crashed host. A live host's socket looks
//! exactly the same, so a second daemon on the same state root took the socket
//! and both then owned the same Runs and the same durable records. A desktop
//! GUI launched twice is the ordinary way to reach that state, not an exotic
//! one, and the consequence is a Run executing twice.

use agent_runtime_host::ipc::{LocalRuntimeDaemon, default_socket_path};
use std::path::PathBuf;
use uuid::Uuid;

fn state_root(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agent-single-{label}-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn a_second_daemon_on_the_same_state_root_is_refused() {
    let root = state_root("refuse");
    let socket = default_socket_path(&root);

    let first = LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("the first daemon must bind");

    let second = LocalRuntimeDaemon::bind(&socket).await;
    assert!(
        second.is_err(),
        "a second daemon took over a live state root, so one Run can execute twice"
    );

    drop(first);
}

/// The other half of the same problem: refusing too eagerly would leave a host
/// permanently unstartable after a crash, which is worse than the defect.
#[tokio::test]
async fn a_socket_left_by_a_dead_daemon_does_not_block_startup() {
    let root = state_root("stale");
    let socket = default_socket_path(&root);

    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("first bind");
    // Dropping the listener ends the daemon exactly as a crash would: the file
    // stays on disk with nothing behind it.
    drop(listener);
    assert!(
        socket.exists(),
        "the test needs a leftover socket to be meaningful"
    );

    LocalRuntimeDaemon::bind(&socket)
        .await
        .expect("a stale socket must not make the host unstartable");
}

/// A clean exit should not leave the socket behind for the next start to
/// reason about at all.
#[tokio::test]
async fn a_clean_shutdown_removes_its_socket() {
    let root = state_root("cleanup");
    let socket = default_socket_path(&root);

    let listener = LocalRuntimeDaemon::bind(&socket).await.expect("bind");
    LocalRuntimeDaemon::release(&socket, listener);

    assert!(
        !socket.exists(),
        "the socket outlived a clean shutdown at {}",
        socket.display()
    );
}
