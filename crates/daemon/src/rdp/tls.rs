//! TLS upgrade of the RDP connection (Enhanced RDP Security).
//!
//! Certificates are verified against the Mozilla root store bundled by
//! `webpki-roots`, unless `ignore-cert` is set, in which case any
//! certificate is accepted (RDP servers usually present a self-signed one).
//! The server's public key is returned because CredSSP binds to it.

use std::io;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use x509_cert::der::Decode as _;

/// Builds the client configuration. Session resumption is disabled because
/// every RDP connection performs a full handshake anyway.
pub fn client_config(ignore_cert: bool) -> io::Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?;
    let mut config = if ignore_cert {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate {
                algorithms: provider.signature_verification_algorithms,
            }))
            .with_no_client_auth()
    } else {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

/// Performs the TLS handshake and returns the stream with the server's
/// public key (the `subjectPublicKey` bits of its certificate).
pub async fn upgrade<S>(
    stream: S,
    hostname: &str,
    ignore_cert: bool,
) -> io::Result<(TlsStream<S>, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let config = Arc::new(client_config(ignore_cert)?);
    let server_name = ServerName::try_from(hostname.to_owned())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let tls = TlsConnector::from(config)
        .connect(server_name, stream)
        .await?;
    let certificate = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or_else(|| io::Error::other("the server sent no certificate"))?;
    let public_key = public_key(certificate)?;
    Ok((tls, public_key))
}

/// Extracts the subject public key of a DER certificate.
pub fn public_key(certificate: &[u8]) -> io::Result<Vec<u8>> {
    let certificate = x509_cert::Certificate::from_der(certificate).map_err(io::Error::other)?;
    certificate
        .tbs_certificate()
        .subject_public_key_info()
        .subject_public_key
        .as_bytes()
        .map(<[u8]>::to_vec)
        .ok_or_else(|| io::Error::other("unaligned subject public key"))
}

/// Accepts any certificate, but still checks handshake signatures so the
/// peer must own the key it presents (which CredSSP relies on).
#[derive(Debug)]
struct AcceptAnyCertificate {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_build_for_both_policies() {
        assert!(client_config(true).is_ok());
        assert!(client_config(false).is_ok());
    }

    #[test]
    fn extracts_the_subject_public_key() {
        use base64::Engine as _;
        // Self-signed P-256 certificate (CN=test).
        const CERT: &str = "MIIBUDCB+KADAgECAhREqGOs2pGEfY/yaHXXSq/Xe3MhpTAKBggqhkjOPQQDAjAPMQ0wCwYDVQQDDAR0\
            ZXN0MB4XDTI2MTEyNDE3NTUxNFoXDTI2MTEyNTE3NTUxNFowDzENMAsGA1UEAwwEdGVzdDBZMBMGByqGSM49\
            AgEGCCqGSM49AwEHA0IABKZh8CV/Yg2OK9pKhDFYgjde3BBkDRY1Lg2CHTo2JQKyqGWpGySpoAObwF/PYNFd\
            kkqKkmMGvAXeVLyzRntipF2jMjAwMB0GA1UdDgQWBBR7zaozBN9DMgKIpBCY1EuzjFGKQjAPBgNVHRMBAf8E\
            BTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIAq3CHEq4fB863XnzxOh7kM2dZqyAFLMM0KeOXpizQCIAiAdfdmC\
            kVRquoeuwJ8Hp/dwskmoqmkKI6hdsQbX8jbxqw==";
        let der = base64::engine::general_purpose::STANDARD
            .decode(CERT)
            .unwrap();
        let key = public_key(&der).unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "04a661f0257f620d8e2bda4a84315882375edc10640d16352e0d821d3a362502b2\
             a865a91b24a9a0039bc05fcf60d15d924a8a926306bc05de54bcb3467b62a45d"
        );
    }

    #[test]
    fn garbage_certificates_are_rejected() {
        assert!(public_key(&[0x30, 0x03, 0x02, 0x01, 0x00]).is_err());
    }
}
