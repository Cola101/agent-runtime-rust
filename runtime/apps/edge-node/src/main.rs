use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().init();
    info!(component = "edge-node", "edge node process initialized");
}
