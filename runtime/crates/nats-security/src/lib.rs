//! Secure NATS connection policy shared by Rust runtime workloads.

use async_nats::{Client, ConnectError, ConnectOptions};
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

#[derive(Clone)]
pub struct NatsClientConfig {
    url: String,
    username: String,
    password: String,
    root_certificates: PathBuf,
}

impl NatsClientConfig {
    pub fn new(
        url: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        root_certificates: PathBuf,
    ) -> Result<Self, NatsSecurityError> {
        let url = required("url", url.into())?;
        let username = required("username", username.into())?;
        let password = required("password", password.into())?;
        if !url.starts_with("tls://") {
            return Err(NatsSecurityError::TlsRequired);
        }
        if root_certificates.as_os_str().is_empty() {
            return Err(NatsSecurityError::BlankField("root_certificates"));
        }
        Ok(Self {
            url,
            username,
            password,
            root_certificates,
        })
    }

    pub async fn connect(&self) -> Result<Client, ConnectError> {
        self.connect_options().connect(self.url.as_str()).await
    }

    pub async fn connect_with_event_callback<F, Fut>(
        &self,
        callback: F,
    ) -> Result<Client, ConnectError>
    where
        F: Fn(async_nats::Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + Sync + 'static,
    {
        self.connect_options()
            .event_callback(callback)
            .connect(self.url.as_str())
            .await
    }

    fn connect_options(&self) -> ConnectOptions {
        ConnectOptions::new()
            .user_and_password(self.username.clone(), self.password.clone())
            .require_tls(true)
            .add_root_certificates(self.root_certificates.clone())
    }
}

impl fmt::Debug for NatsClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NatsClientConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("root_certificates", &self.root_certificates)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NatsSecurityError {
    #[error("{0} must not be blank")]
    BlankField(&'static str),
    #[error("NATS transport must use a tls:// URL")]
    TlsRequired,
}

fn required(field: &'static str, value: String) -> Result<String, NatsSecurityError> {
    if value.trim().is_empty() {
        Err(NatsSecurityError::BlankField(field))
    } else {
        Ok(value)
    }
}
