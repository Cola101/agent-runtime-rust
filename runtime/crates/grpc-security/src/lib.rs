use std::path::Path;
use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GrpcSecurityConfigurationError {
    #[error("{0} PEM material must not be blank")]
    BlankMaterial(&'static str),
    #[error("gRPC TLS domain name must not be blank")]
    BlankDomainName,
    #[error("failed to read {kind} PEM file: {message}")]
    ReadFile { kind: &'static str, message: String },
}

#[derive(Clone, Debug)]
pub struct ServerMtlsMaterials {
    certificate_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
    client_ca_pem: Vec<u8>,
}

impl ServerMtlsMaterials {
    pub fn new(
        certificate_pem: Vec<u8>,
        private_key_pem: Vec<u8>,
        client_ca_pem: Vec<u8>,
    ) -> Result<Self, GrpcSecurityConfigurationError> {
        require_pem("server certificate", &certificate_pem)?;
        require_pem("server private key", &private_key_pem)?;
        require_pem("client CA certificate", &client_ca_pem)?;
        Ok(Self {
            certificate_pem,
            private_key_pem,
            client_ca_pem,
        })
    }

    pub fn from_files(
        certificate_path: impl AsRef<Path>,
        private_key_path: impl AsRef<Path>,
        client_ca_path: impl AsRef<Path>,
    ) -> Result<Self, GrpcSecurityConfigurationError> {
        Self::new(
            read_file("server certificate", certificate_path)?,
            read_file("server private key", private_key_path)?,
            read_file("client CA certificate", client_ca_path)?,
        )
    }

    #[must_use]
    pub fn into_tonic(self) -> ServerTlsConfig {
        ServerTlsConfig::new()
            .identity(Identity::from_pem(
                self.certificate_pem,
                self.private_key_pem,
            ))
            .client_ca_root(Certificate::from_pem(self.client_ca_pem))
    }
}

#[derive(Clone, Debug)]
pub struct ClientMtlsMaterials {
    certificate_pem: Vec<u8>,
    private_key_pem: Vec<u8>,
    server_ca_pem: Vec<u8>,
    domain_name: String,
}

impl ClientMtlsMaterials {
    pub fn new(
        certificate_pem: Vec<u8>,
        private_key_pem: Vec<u8>,
        server_ca_pem: Vec<u8>,
        domain_name: String,
    ) -> Result<Self, GrpcSecurityConfigurationError> {
        require_pem("client certificate", &certificate_pem)?;
        require_pem("client private key", &private_key_pem)?;
        require_pem("server CA certificate", &server_ca_pem)?;
        if domain_name.trim().is_empty() {
            return Err(GrpcSecurityConfigurationError::BlankDomainName);
        }
        Ok(Self {
            certificate_pem,
            private_key_pem,
            server_ca_pem,
            domain_name,
        })
    }

    pub fn from_files(
        certificate_path: impl AsRef<Path>,
        private_key_path: impl AsRef<Path>,
        server_ca_path: impl AsRef<Path>,
        domain_name: String,
    ) -> Result<Self, GrpcSecurityConfigurationError> {
        Self::new(
            read_file("client certificate", certificate_path)?,
            read_file("client private key", private_key_path)?,
            read_file("server CA certificate", server_ca_path)?,
            domain_name,
        )
    }

    #[must_use]
    pub fn into_tonic(self) -> ClientTlsConfig {
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(self.server_ca_pem))
            .identity(Identity::from_pem(
                self.certificate_pem,
                self.private_key_pem,
            ))
            .domain_name(self.domain_name)
    }
}

fn require_pem(kind: &'static str, material: &[u8]) -> Result<(), GrpcSecurityConfigurationError> {
    if material.iter().all(u8::is_ascii_whitespace) {
        return Err(GrpcSecurityConfigurationError::BlankMaterial(kind));
    }
    Ok(())
}

fn read_file(
    kind: &'static str,
    path: impl AsRef<Path>,
) -> Result<Vec<u8>, GrpcSecurityConfigurationError> {
    std::fs::read(path).map_err(|error| GrpcSecurityConfigurationError::ReadFile {
        kind,
        message: error.to_string(),
    })
}
