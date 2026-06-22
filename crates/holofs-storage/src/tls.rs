//! TLS scaffold for the node↔gateway wire protocol.
//!
//! This module knows how to:
//! - Generate a self-signed CA + leaf certificate (used in embedded mode so
//!   the binary works out of the box without external PKI).
//! - Load existing PEM cert + key + CA bundle from disk (distributed mode).
//! - Build [`rustls::ServerConfig`] / [`rustls::ClientConfig`] for the
//!   node service and the gateway client respectively.
//!
//! TLS is **opt-in** at the binary level. When the gateway and nodes are
//! launched without `--tls`, they keep speaking plain TCP — same as before
//! Stage 6. Once `--tls` is set the wire is encrypted; passing `--mtls`
//! further requires + verifies a client certificate.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// PEM bytes of a CA-style certificate + matching private key. Held in
/// memory; constructors below either generate a fresh pair (in-RAM only)
/// or load PEM files from disk.
#[derive(Debug, Clone)]
pub struct CaPair {
    /// CA certificate in PEM form (begin/end CERTIFICATE).
    pub ca_cert_pem: String,
    /// CA private key in PEM form (begin/end PRIVATE KEY).
    pub ca_key_pem: String,
}

/// One node's leaf certificate signed by [`CaPair`]. Both `cert_pem` and
/// `key_pem` are PEM-encoded so they fit the same file-on-disk layout as
/// the distributed-mode operator-supplied certs.
#[derive(Debug, Clone)]
pub struct NodeIdentityCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// What the binary discovered on disk / generated at boot. The wire
/// frontend (gateway client) and the node server both pull from this
/// snapshot — there's no global state, all configs are passed explicitly.
#[derive(Debug, Clone)]
pub struct TlsMaterial {
    /// CA used to sign every leaf cert (server + optional client).
    pub ca: CaPair,
    /// The leaf cert this binary presents as its TLS identity. For a
    /// node, this is its server cert (SNI = node name). For the gateway
    /// under mTLS, this is its client cert.
    pub leaf: NodeIdentityCert,
}

/// Held alongside a freshly-generated [`TlsMaterial`] so bootstrap can mint
/// per-node leaf certs without re-parsing PEM. Construction from on-disk
/// PEM does not produce a signer (the rcgen object graph is not
/// recoverable from `rcgen 0.13` PEM input).
pub struct CaSigner {
    /// The live rcgen Certificate the issuer signs new leaves against.
    pub(crate) ca_cert: rcgen::Certificate,
    pub(crate) ca_kp: rcgen::KeyPair,
}

impl CaSigner {
    /// Sign a fresh leaf with `subject_name` (CN) and `sans` (SubjectAltName).
    /// Each SAN entry should either be a DNS label (e.g. `node-7`) or an
    /// IPv4/IPv6 string (e.g. `127.0.0.1`).
    pub fn issue_leaf(
        &self,
        subject_name: &str,
        sans: &[String],
    ) -> Result<NodeIdentityCert, rcgen::Error> {
        let mut params = rcgen::CertificateParams::new(sans.to_vec())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, subject_name);
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        let leaf_kp = rcgen::KeyPair::generate()?;
        let cert = params.signed_by(&leaf_kp, &self.ca_cert, &self.ca_kp)?;
        Ok(NodeIdentityCert {
            cert_pem: cert.pem(),
            key_pem: leaf_kp.serialize_pem(),
        })
    }
}

impl TlsMaterial {
    /// Read PEM cert+key+ca from disk paths. Errors if any file is missing
    /// or malformed — this is intentional: distributed deployments must
    /// not silently fall back to a generated CA.
    pub fn load(
        leaf_cert: &Path,
        leaf_key: &Path,
        ca_cert: &Path,
        ca_key: Option<&Path>,
    ) -> io::Result<Self> {
        let cert_pem = fs::read_to_string(leaf_cert)?;
        let key_pem = fs::read_to_string(leaf_key)?;
        let ca_cert_pem = fs::read_to_string(ca_cert)?;
        let ca_key_pem = match ca_key {
            Some(p) => fs::read_to_string(p)?,
            // mTLS does not need the CA key on the runtime path — only the
            // CA cert. Loading without the key produces a valid TlsMaterial
            // for verification, but `issue_leaf` will fail.
            None => String::new(),
        };
        Ok(Self {
            ca: CaPair {
                ca_cert_pem,
                ca_key_pem,
            },
            leaf: NodeIdentityCert { cert_pem, key_pem },
        })
    }

    /// Generate a brand-new self-signed CA + leaf cert with `subject_name`
    /// in its SAN list. Used in embedded mode where the gateway and nodes
    /// share a process and trust the same CA.
    ///
    /// The returned [`CaSigner`] keeps the CA's signing key live in memory
    /// so the bootstrap can mint additional per-node leaves cheaply
    /// (`signer.issue_leaf(...)`).
    pub fn self_signed(
        subject_name: &str,
        sans: &[String],
    ) -> Result<(Self, CaSigner), rcgen::Error> {
        let (ca_pair, signer) = generate_ca()?;
        let leaf = signer.issue_leaf(subject_name, sans)?;
        Ok((
            Self {
                ca: ca_pair,
                leaf,
            },
            signer,
        ))
    }

    /// Write all four PEM files under `dir` (`ca.crt`, `ca.key`, `<name>.crt`,
    /// `<name>.key`). Mostly useful for embedded mode so an operator can
    /// inspect the generated material with `openssl`.
    pub fn write_to_dir(&self, dir: &Path, leaf_name: &str) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("ca.crt"), &self.ca.ca_cert_pem)?;
        fs::write(dir.join("ca.key"), &self.ca.ca_key_pem)?;
        fs::write(dir.join(format!("{leaf_name}.crt")), &self.leaf.cert_pem)?;
        fs::write(dir.join(format!("{leaf_name}.key")), &self.leaf.key_pem)?;
        Ok(())
    }

    /// Build the rustls [`ServerConfig`] this material implies. `mtls = true`
    /// makes the server require + verify a client cert signed by `self.ca`.
    pub fn server_config(&self, mtls: bool) -> Result<Arc<ServerConfig>, TlsBuildError> {
        let certs = parse_certs(&self.leaf.cert_pem)?;
        let key = parse_private_key(&self.leaf.key_pem)?;
        let builder = ServerConfig::builder();
        let builder = if mtls {
            let mut roots = RootCertStore::empty();
            for c in parse_certs(&self.ca.ca_cert_pem)? {
                roots
                    .add(c)
                    .map_err(|e| TlsBuildError::Other(e.to_string()))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| TlsBuildError::Other(e.to_string()))?;
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let cfg = builder
            .with_single_cert(certs, key)
            .map_err(|e| TlsBuildError::Other(e.to_string()))?;
        Ok(Arc::new(cfg))
    }

    /// Build the rustls [`ClientConfig`] this material implies. The CA is
    /// the trust root; under mTLS the client also presents its leaf cert.
    pub fn client_config(&self, mtls: bool) -> Result<Arc<ClientConfig>, TlsBuildError> {
        let mut roots = RootCertStore::empty();
        for c in parse_certs(&self.ca.ca_cert_pem)? {
            roots
                .add(c)
                .map_err(|e| TlsBuildError::Other(e.to_string()))?;
        }
        let builder = ClientConfig::builder().with_root_certificates(roots);
        let cfg = if mtls {
            let certs = parse_certs(&self.leaf.cert_pem)?;
            let key = parse_private_key(&self.leaf.key_pem)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| TlsBuildError::Other(e.to_string()))?
        } else {
            builder.with_no_client_auth()
        };
        Ok(Arc::new(cfg))
    }
}

/// Parse a `host:port` string into the `ServerName` rustls wants. We use
/// the host part as the SNI; for raw IPs rustls expects `ServerName::IpAddress`.
pub fn server_name_for(host_port: &str) -> Result<ServerName<'static>, TlsBuildError> {
    let host = host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        Ok(ServerName::IpAddress(ip.into()))
    } else {
        ServerName::try_from(host.to_string())
            .map_err(|e| TlsBuildError::Other(format!("bad SNI host {host:?}: {e}")))
    }
}

#[derive(Debug)]
pub enum TlsBuildError {
    Pem(String),
    Other(String),
}

impl std::fmt::Display for TlsBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsBuildError::Pem(s) => write!(f, "pem parse: {s}"),
            TlsBuildError::Other(s) => write!(f, "tls config: {s}"),
        }
    }
}

impl std::error::Error for TlsBuildError {}

fn parse_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, TlsBuildError> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsBuildError::Pem(e.to_string()))
}

fn parse_private_key(pem: &str) -> Result<PrivateKeyDer<'static>, TlsBuildError> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes())
        .map_err(|e| TlsBuildError::Pem(e.to_string()))
}

fn generate_ca() -> Result<(CaPair, CaSigner), rcgen::Error> {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "holofs embedded CA");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let ca_kp = rcgen::KeyPair::generate()?;
    let ca_cert = params.self_signed(&ca_kp)?;
    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_kp.serialize_pem();
    Ok((
        CaPair {
            ca_cert_pem,
            ca_key_pem,
        },
        CaSigner { ca_cert, ca_kp },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_roundtrip() {
        let (mat, _signer) =
            TlsMaterial::self_signed("node-0", &["node-0".into(), "127.0.0.1".into()]).unwrap();
        assert!(mat.ca.ca_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(mat.leaf.cert_pem.contains("BEGIN CERTIFICATE"));
        // Both configs must build cleanly under both mTLS modes.
        let _server_plain = mat.server_config(false).unwrap();
        let _server_mtls = mat.server_config(true).unwrap();
        let _client_plain = mat.client_config(false).unwrap();
        let _client_mtls = mat.client_config(true).unwrap();
    }

    #[test]
    fn issue_extra_leaf() {
        let (mat, signer) =
            TlsMaterial::self_signed("ca-root", &["127.0.0.1".into()]).unwrap();
        let leaf2 = signer
            .issue_leaf("node-7", &["node-7".into(), "127.0.0.1".into()])
            .unwrap();
        assert!(leaf2.cert_pem.contains("BEGIN CERTIFICATE"));
        assert_ne!(leaf2.cert_pem, mat.leaf.cert_pem);
    }
}
