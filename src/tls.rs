use anyhow::Context;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use rustls::server::danger::ClientCertVerifier;
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use std::fmt::{Display, Formatter};
use std::path::PathBuf;
use std::sync::Arc;

/// Available relay transport protocols.
pub enum Protocol {
    Http2,
    Http3,
}

impl Display for Protocol {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http2 => write!(f, "h2"),
            Self::Http3 => write!(f, "h3"),
        }
    }
}

impl From<Protocol> for Vec<u8> {
    fn from(value: Protocol) -> Self {
        Self::from(value.to_string())
    }
}

/// Base TLS configuration that contains everything to create the final TLS configuration. This
/// allows only loading certificates and private keys once.
pub struct BaseServerConfig {
    client_verifier: Arc<dyn ClientCertVerifier>,
    server_chain: Vec<CertificateDer<'static>>,
    server_key: PrivateKeyDer<'static>,
}

impl Clone for BaseServerConfig {
    fn clone(&self) -> Self {
        Self {
            client_verifier: self.client_verifier.clone(),
            server_chain: self.server_chain.clone(),
            server_key: self.server_key.clone_key(),
        }
    }
}

/// Builds a base TLS config with:
/// - Server certificate in `{directory}/server.pem`
/// - Server private key in `{directory}/server.key`
/// - Client certificate authority in `{directory}/client.pem`
/// - Client certificate revocation list in `{directory}/client.crl`
///
/// Use [finish_config] to make the final TLS config.
pub fn tls_config(directory: impl Into<PathBuf>) -> anyhow::Result<BaseServerConfig> {
    let directory = directory.into();

    // Load the server certificate chain
    let server_chain: Vec<_> = CertificateDer::pem_file_iter(directory.join("server.pem"))
        .context("while reading server certificate file (`server.pem`)")?
        .flatten()
        .collect();

    // Load the server private key
    let server_key = PrivateKeyDer::from_pem_file(directory.join("server.key"))
        .context("while reading server private key file (`server.key`)")?;

    // Load the root certificate chain that signs client certificates
    let client_chain = CertificateDer::pem_file_iter(directory.join("client.pem"))
        .context("while reading client certificate file (`client.pem`)")?
        .flatten();

    // Load each root certificate into a RootCertStore
    let mut client_roots = RootCertStore::empty();
    for (index, certificate) in client_chain.enumerate() {
        client_roots
            .add(certificate)
            .with_context(|| format!("while processing client certificate {index}"))?;
    }

    // Load the certificate revocation list
    let client_revocations =
        CertificateRevocationListDer::pem_file_iter(directory.join("client.crl"))
            .context("while reading client certificate revocations file (`client.crl`)")?
            .flatten();

    // Create the verifier that verifies certificates of connecting clients
    let client_verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
        .with_crls(client_revocations)
        .build()
        .context("while creating WebPkiClientVerifier")?;

    Ok(BaseServerConfig {
        client_verifier,
        server_chain,
        server_key,
    })
}

/// Finalizes a [BaseServerConfig] by extending it with a [Protocol].
pub fn finish_config(config: BaseServerConfig, protocol: Protocol) -> anyhow::Result<ServerConfig> {
    let mut config = ServerConfig::builder()
        // .with_client_cert_verifier(config.client_verifier)
        .with_no_client_auth() // TODO TEMP
        .with_single_cert(config.server_chain, config.server_key)
        .context("while creating TLS server config")?;

    config.alpn_protocols = vec![protocol.into()];
    config.max_early_data_size = u32::MAX;

    Ok(config)
}
