use agent_runtime_health::{HealthState, serve};
use reqwest::StatusCode;
use tokio::net::TcpListener;

#[tokio::test]
async fn readiness_tracks_traffic_acceptance_without_breaking_liveness() {
    let state = HealthState::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, state.clone()));
    let client = reqwest::Client::new();

    assert_eq!(
        client
            .get(format!("http://{address}/live"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("http://{address}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );

    state.set_ready(true);
    assert_eq!(
        client
            .get(format!("http://{address}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    state.set_ready(false);
    assert_eq!(
        client
            .get(format!("http://{address}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        client
            .get(format!("http://{address}/missing"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    server.abort();
}
