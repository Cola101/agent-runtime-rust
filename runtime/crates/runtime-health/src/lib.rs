//! Semantic liveness and readiness endpoints for runtime workloads.

use axum::{Router, http::StatusCode, routing::get};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::net::TcpListener;

/// Process-local traffic acceptance state.
///
/// Liveness deliberately does not depend on this value: a disconnected
/// dependency should remove the pod from service without causing a restart
/// loop. Readiness is safe to update from background dependency monitors.
#[derive(Clone, Default)]
pub struct HealthState {
    ready: Arc<AtomicBool>,
}

impl HealthState {
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

pub async fn serve(listener: TcpListener, state: HealthState) -> io::Result<()> {
    let readiness = state.clone();
    let router = Router::new()
        .route("/live", get(|| async { StatusCode::OK }))
        .route(
            "/ready",
            get(move || {
                let readiness = readiness.clone();
                async move {
                    if readiness.is_ready() {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
    axum::serve(listener, router).await
}
