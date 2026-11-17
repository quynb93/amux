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
    let dir = paths::AMUX_HOME.join("tls");
    crate::fsx::mkdir_private(&dir)?;
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
    if cert.exists() && key.exists() {
        return Ok((cert, key));
    }
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
            "-subj",
            "/CN=amux serve",
        ])
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .arg("-addext")
        .arg(format!("subjectAltName={}", sans.join(",")))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("could not run openssl to make a certificate (or pass --cert and --key)")?;
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
    }
}
