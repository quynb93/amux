//! HTTPS for `amux serve`: a connection that is plain TCP or TLS, and where certificates
//! come from.
//!
//! - `--tls`: a self-signed certificate, made once with `openssl` and kept in
//!   ~/.amux/tls/. Encrypted, but a browser warns until you trust it; the fingerprint is
//!   printed so you can check it's yours.
//! - `--cert <pem> --key <pem>`: your own (from `tailscale cert`, mkcert, Let's Encrypt…).
//! - `--tailscale` doesn't use this file: Tailscale terminates TLS with a real certificate
//!   for the machine's `*.ts.net` name and passes requests to amux on 127.0.0.1.

use crate::paths;
use anyhow::{bail, Context, Result};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

/// One connection, plain or TLS. A connection is handled on one thread, reading the
/// request and then writing the answer, so `&Conn` reads and writes like `&TcpStream`.
pub struct Conn(RefCell<Box<dyn Io>>);

impl Conn {
    pub fn plain(s: TcpStream) -> Conn {
        Conn(RefCell::new(Box::new(s)))
    }

    /// The TLS handshake happens on the first read.
    pub fn tls(s: TcpStream, cfg: &Arc<rustls::ServerConfig>) -> Result<Conn> {
        let conn = rustls::ServerConnection::new(cfg.clone())?;
        Ok(Conn(RefCell::new(Box::new(rustls::StreamOwned::new(
            conn, s,
        )))))
    }
}

impl Read for &Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(buf)
    }
}

impl Write for &Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// A TLS server setup from a PEM certificate chain and key.
pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("could not read {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("{} is not a PEM certificate", cert.display()))?;
    if certs.is_empty() {
        bail!("{} has no certificate in it", cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("could not read a private key from {}", key.display()))?;
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("the certificate and key don't go together")?;
    Ok(Arc::new(cfg))
}

/// The self-signed certificate for these names and addresses, made the first time.
/// Returns (cert, key).
pub fn self_signed(names: &[String]) -> Result<(PathBuf, PathBuf)> {
    self_signed_in(&paths::AMUX_HOME.join("tls"), names)
}

fn self_signed_in(dir: &Path, names: &[String]) -> Result<(PathBuf, PathBuf)> {
    crate::fsx::mkdir_private(dir)?;
    let mut sans: Vec<String> = vec!["DNS:localhost".into(), "IP:127.0.0.1".into()];
    for n in names {
        let san = if n.parse::<std::net::IpAddr>().is_ok() {
            format!("IP:{n}")
        } else {
            format!("DNS:{n}")
        };
        if !sans.contains(&san) {
            sans.push(san);
        }
    }
    // One certificate per set of names, so a new address gets a certificate that has it.
    let id: String = {
        use sha2::Digest;
        let h = sha2::Sha256::digest(sans.join(",").as_bytes());
        h.iter().take(4).map(|b| format!("{b:02x}")).collect()
    };
    let (cert, key) = (
        dir.join(format!("self-{id}.pem")),
        dir.join(format!("self-{id}.key")),
    );
    // Certificates made before 0.11.15 were CA certificates (openssl's default for
    // `req -x509`): trusting one on a phone let whoever holds the key vouch for any site.
    // Those are made again as server-only certificates.
    if cert.exists() && key.exists() && cert_is_leaf(&cert) {
        return Ok((cert, key));
    }
    // A config of our own, so openssl's default `v3_ca` extensions don't apply.
    let cnf = dir.join(format!("self-{id}.cnf"));
    std::fs::write(
        &cnf,
        format!(
            "[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n\
             [dn]\nCN=amux serve\n\
             [v3]\nbasicConstraints=critical,CA:FALSE\n\
             keyUsage=critical,digitalSignature\n\
             extendedKeyUsage=serverAuth\n\
             subjectKeyIdentifier=hash\n\
             subjectAltName={}\n",
            sans.join(",")
        ),
    )?;
    let ok = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:prime256v1",
            "-nodes",
            "-days",
            "825",
        ])
        .arg("-config")
        .arg(&cnf)
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let _ = std::fs::remove_file(&cnf);
    let ok =
        ok.context("could not run openssl to make a certificate (or pass --cert and --key)")?;
    if !ok.status.success() {
        let _ = std::fs::remove_file(&key);
        bail!(
            "openssl could not make a certificate: {}",
            String::from_utf8_lossy(&ok.stderr).trim()
        );
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    Ok((cert, key))
}

/// Whether a PEM certificate's first certificate says it is not a CA: it has the
/// basicConstraints extension and `cA` is false. A missing extension counts as not a leaf,
/// so an old certificate without it is made again.
pub fn cert_is_leaf(cert: &Path) -> bool {
    let Some(Ok(der)) = CertificateDer::pem_file_iter(cert)
        .ok()
        .and_then(|mut it| it.next())
    else {
        return false;
    };
    der_is_leaf(der.as_ref())
}

/// In DER: the basicConstraints OID (2.5.29.19), an optional `critical` BOOLEAN, then an
/// OCTET STRING holding `SEQUENCE { cA BOOLEAN DEFAULT FALSE, ... }`.
fn der_is_leaf(der: &[u8]) -> bool {
    const OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1d, 0x13];
    let Some(at) = der.windows(OID.len()).position(|w| w == OID) else {
        return false;
    };
    let mut i = at + OID.len();
    if der.get(i) == Some(&0x01) {
        i += 3; // BOOLEAN, length 1, value
    }
    if der.get(i) != Some(&0x04) {
        return false;
    }
    i += 2; // OCTET STRING and a short length
    if der.get(i) != Some(&0x30) {
        return false;
    }
    let len = der.get(i + 1).copied().unwrap_or(0);
    // An empty sequence: cA is FALSE by default. Otherwise cA is the first element.
    len == 0 || !(der.get(i + 2) == Some(&0x01) && der.get(i + 4).is_some_and(|v| *v != 0))
}

/// The SHA-256 fingerprint of a PEM certificate's first certificate, as browsers show it.
pub fn fingerprint(cert: &Path) -> Option<String> {
    use sha2::Digest;
    let der = CertificateDer::pem_file_iter(cert).ok()?.next()?.ok()?;
    let h = sha2::Sha256::digest(der.as_ref());
    Some(
        h.iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// The Tailscale CLI: on PATH, or inside the Mac app.
pub fn tailscale_bin() -> Option<String> {
    if let Ok(p) = std::env::var("AMUX_TAILSCALE") {
        return Some(p);
    }
    let on_path = Command::new("sh")
        .args(["-c", "command -v tailscale"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    on_path.or_else(|| {
        let app = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
        Path::new(app).exists().then(|| app.to_string())
    })
}

/// This machine's name on the tailnet (`box.tail1234.ts.net`).
pub fn tailscale_name(bin: &str) -> Result<String> {
    let out = Command::new(bin)
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .output()
        .context("could not run tailscale")?;
    if !out.status.success() {
        bail!(
            "tailscale status failed (is Tailscale up?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("unexpected output from tailscale status")?;
    let name = v["Self"]["DNSName"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('.');
    if name.is_empty() {
        bail!("Tailscale has no name for this machine: turn on MagicDNS in the admin console.");
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn loads_a_certificate_and_fingerprints_it() {
        let t = TempDir::new();
        let (cert, key) = (t.path().join("c.pem"), t.path().join("k.pem"));
        let ok = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "ec", "-pkeyopt"])
            .args(["ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
            .args(["-subj", "/CN=t", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(ok.success());
        assert!(server_config(&cert, &key).is_ok());
        assert!(
            server_config(&key, &key).is_err(),
            "a key is not a certificate"
        );
        let fp = fingerprint(&cert).unwrap();
        assert_eq!(fp.len(), 32 * 3 - 1);
        // openssl's default for `req -x509` is a CA certificate.
        assert!(!cert_is_leaf(&cert));
    }

    #[test]
    fn self_signed_certificate_is_a_server_certificate_not_a_ca() {
        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("skipped: no openssl");
            return;
        }
        let t = TempDir::new();
        let dir = t.path().join("tls");
        let (cert, key) = self_signed_in(&dir, &["192.168.1.5".to_string()]).unwrap();
        assert!(cert_is_leaf(&cert));
        assert!(server_config(&cert, &key).is_ok());
        let text = Command::new("openssl")
            .args(["x509", "-noout", "-text", "-in"])
            .arg(&cert)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&text.stdout);
        assert!(text.contains("CA:FALSE"), "{text}");
        assert!(!text.contains("CA:TRUE"), "{text}");
        assert!(text.contains("TLS Web Server Authentication"), "{text}");
        assert!(text.contains("192.168.1.5"), "{text}");
        // An old CA certificate under the same name is replaced on the next start.
        let ok = Command::new("openssl")
            .args(["req", "-x509", "-newkey", "ec", "-pkeyopt"])
            .args(["ec_paramgen_curve:prime256v1", "-nodes", "-days", "1"])
            .args(["-subj", "/CN=old", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(ok.success() && !cert_is_leaf(&cert));
        let (cert2, _) = self_signed_in(&dir, &["192.168.1.5".to_string()]).unwrap();
        assert_eq!(cert2, cert);
        assert!(cert_is_leaf(&cert));
    }
}
