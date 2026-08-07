use agent_nats_security::NatsClientConfig;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "requires a live TLS-enabled NATS server"]
async fn authenticates_over_tls_and_rejects_invalid_credentials() {
    let url = std::env::var("TEST_NATS_URL").unwrap();
    let username = std::env::var("TEST_NATS_USERNAME").unwrap();
    let password = std::env::var("TEST_NATS_PASSWORD").unwrap();
    let ca = PathBuf::from(std::env::var("TEST_NATS_CA_CERT").unwrap());

    let valid = NatsClientConfig::new(&url, &username, &password, ca.clone()).unwrap();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let client = valid
        .connect_with_event_callback(move |event| {
            let events_tx = events_tx.clone();
            async move {
                let _ = events_tx.send(event);
            }
        })
        .await
        .unwrap();
    client
        .publish("runtime.worker.health.test", "ok".into())
        .await
        .unwrap();
    client.flush().await.unwrap();

    // The worker account must not cross the platform role boundary.
    client
        .publish("runtime.control.forbidden", "denied".into())
        .await
        .unwrap();
    let _ = client.flush().await;
    let authorization_violation = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(async_nats::Event::ServerError(_)) = events_rx.recv().await {
                break;
            }
        }
    })
    .await;
    assert!(
        authorization_violation.is_ok(),
        "worker account published to runtime.control.>"
    );

    client
        .publish("$JS.API.STREAM.DELETE.RUNTIME_EXECUTION", "denied".into())
        .await
        .unwrap();
    let _ = client.flush().await;
    let stream_admin_violation = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(async_nats::Event::ServerError(_)) = events_rx.recv().await {
                break;
            }
        }
    })
    .await;
    assert!(
        stream_admin_violation.is_ok(),
        "worker account can invoke JetStream stream administration"
    );

    let invalid = NatsClientConfig::new(url, username, "wrong-password", ca).unwrap();
    assert!(invalid.connect().await.is_err());
}
