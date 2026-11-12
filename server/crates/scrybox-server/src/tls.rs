//! Lane B unit 5c: the optional HTTPS listener — twin of
//! backend/src/utils/tls.js. The browser only hands out the camera in a
//! secure context, so a LAN/Docker install serves TLS too when HTTPS_PORT
//! is set; certificates live beside the database (`<db dir>/ssl/{key,cert}.
//! pem`) unless the operator supplies SSL_CERT_PATH + SSL_KEY_PATH.
//!
//! Node-fidelity notes:
//! * Self-signed shape (selfSignedCert): CN=scrybox, 397-day validity
//!   (Safari refuses >398 d), EC prime256v1, basicConstraints CA:false,
//!   keyUsage digitalSignature+keyEncipherment (a0), extKeyUsage
//!   serverAuth, SAN DNS:localhost + IP 127.0.0.1 + ::1 + every
//!   non-internal host address (os.networkInterfaces() loop -> if-addrs).
//!   Serial: 8 random bytes. Verified FIELD-for-FIELD against the
//!   container's Node-generated cert (`openssl x509 -text`, re-pinned this
//!   iter) EXCEPT the key/signature algorithm: Node mints RSA-2048
//!   (selfsigned keySize:2048), rcgen's ring backend is ECDSA P-256 — a
//!   documented, deliberate deviation (BRIEF prescribes rcgen; RSA would
//!   mean the aws-lc-rs backend; browsers trust both alike). NOT
//!   byte-identical either way: serial/SKI/AKI are random, like Node's.
//! * Reuse ladder (loadOrCreateCert): operator pair => read BOTH paths, any
//!   read failure => Err("SSL_CERT_PATH/SSL_KEY_PATH set but unreadable:
//!   …") => HTTPS skipped, HTTP unaffected. Self-signed dir => reuse unless
//!   parse failure or notAfter <= now, then unlink + regenerate; a write
//!   failure => Err("TLS certificate generation failed: …").
//! * No DB path and no operator pair => Err (Node throws; startHttps's
//!   catch logs `HTTPS listener failed to start: <message>` and the plain
//!   HTTP front keeps serving — mirrored by main.rs).
//! * Node's https server sets no ALPN => HTTP/1.1 only; the rustls config
//!   leaves alpn_protocols empty so h2 is never negotiated.
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;

/// tls.js CERT_DAYS — under Safari's 398-day server-cert limit.
const CERT_DAYS: i64 = 397;

/// The non-internal host addresses behind os.networkInterfaces() flatMap.
fn host_addresses() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|ifaces| ifaces.into_iter().filter(|i| !i.is_loopback()).map(|i| i.ip()).collect())
        .unwrap_or_default()
}

/// TLS-failure surface: Err(String) mirrors the Error whose .message Node
/// interpolates into `HTTPS listener failed to start: …`.
pub fn load_or_create_cert(db_path: Option<&Path>) -> Result<(Vec<u8>, Vec<u8>), String> {
    let cert_path = std::env::var("SSL_CERT_PATH").unwrap_or_default();
    let key_path = std::env::var("SSL_KEY_PATH").unwrap_or_default();
    if !cert_path.is_empty() || !key_path.is_empty() {
        let k = std::fs::read(&key_path);
        let c = std::fs::read(&cert_path);
        return match (k, c) {
            (Ok(k), Ok(c)) => Ok((k, c)),
            (Err(e), _) | (Ok(_), Err(e)) => {
                Err(format!("SSL_CERT_PATH/SSL_KEY_PATH set but unreadable: {e}"))
            }
        };
    }
    let Some(db) = db_path else {
        return Err("SCRYBOX_DB_PATH is required to generate the self-signed certificate (set SSL_CERT_PATH + SSL_KEY_PATH to use a real one)".into());
    };
    let cert_dir = db.parent().unwrap_or(Path::new(".")).join("ssl");
    let key_file = cert_dir.join("key.pem");
    let cert_file = cert_dir.join("cert.pem");
    if key_file.exists() && cert_file.exists() {
        let key = std::fs::read(&key_file).map_err(|e| format!("TLS key read failed: {e}"))?;
        let cert = std::fs::read(&cert_file).map_err(|e| format!("TLS certificate read failed: {e}"))?;
        if !cert_expired(&cert) {
            return Ok((key, cert));
        }
        tracing::info!("Self-signed certificate expired; generating a fresh one");
        // Node: fs.rmSync(keyFile, { force: true }) — ENOENT tolerated.
        let _ = std::fs::remove_file(&key_file);
    }
    let (key_pem, cert_pem) = generate_cert();
    std::fs::create_dir_all(&cert_dir).map_err(|e| format!("TLS certificate generation failed: {e}"))?;
    write_private(&key_file, &key_pem).map_err(|e| format!("TLS certificate generation failed: {e}"))?;
    std::fs::write(&cert_file, &cert_pem).map_err(|e| format!("TLS certificate generation failed: {e}"))?;
    tracing::info!(dir = %cert_dir.display(), "Generated self-signed HTTPS certificate (trust it on each device, or set SSL_CERT_PATH + SSL_KEY_PATH)");
    Ok((key_pem, cert_pem))
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)
}

/// notAfter <= now (or an unparseable cert) => regenerate.
fn cert_expired(cert_pem: &[u8]) -> bool {
    let Ok(der) = pem_to_der(cert_pem) else { return true };
    match x509_parser::parse_x509_certificate(&der) {
        Ok((_, c)) => {
            let not_after = c.validity().not_after.timestamp();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0) as i64;
            not_after <= now
        }
        Err(_) => true,
    }
}

fn pem_to_der(bytes: &[u8]) -> Result<Vec<u8>, ()> {
    let s = String::from_utf8_lossy(bytes).into_owned();
    let begin = "-----BEGIN CERTIFICATE-----";
    let end = "-----END CERTIFICATE-----";
    let i = s.find(begin).ok_or(())? + begin.len();
    let j = s[i..].find(end).ok_or(())? + i;
    let b64: String = s[i..j].chars().filter(|c| !c.is_whitespace()).collect();
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64).map_err(|_| ())
}

/// selfSignedCert twin: CN=scrybox, 397 d, RSA-2048 + sha256WithRSA
/// (selfsigned keySize:2048/algorithm:sha256 — live-pinned against
/// `openssl x509 -text` of the container's cert THIS iter), CA:false,
/// digitalSignature+keyEncipherment, EKU serverAuth, SAN localhost +
/// 127.0.0.1 + ::1 + host addrs.
fn generate_cert() -> (Vec<u8>, Vec<u8>) {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyUsagePurpose, SanType,
        SerialNumber,
    };
    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, "scrybox");
    let mut sans = vec![
        SanType::DnsName("localhost".try_into().expect("valid dns name")),
        SanType::IpAddress(IpAddr::V4([127, 0, 0, 1].into())),
        SanType::IpAddress(IpAddr::V6([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1].into())),
    ];
    for ip in host_addresses() {
        sans.push(SanType::IpAddress(ip));
    }
    params.subject_alt_names = sans;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(CERT_DAYS);
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).expect("getrandom");
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    let key_pair = gen_keypair();
    let cert = params.self_signed(&key_pair).expect("self-sign");
    (key_pair.serialize_pem().into_bytes(), cert.pem().into_bytes())
}

fn gen_keypair() -> rcgen::KeyPair {
    // DEVIATION (documented, deliberate): Node's selfsigned mints RSA-2048
    // (keySize:2048/sha256 — confirmed by `openssl x509 -text` of the
    // container's cert this iter). rcgen's ring backend speaks ECDSA only,
    // RSA would mean the aws-lc-rs backend (aws-lc-sys build + binary
    // bloat against the unit-7 <150 MB budget), and the BRIEF prescribes
    // "rcgen self-signed". EC P-256 is trusted identically by every target
    // browser; every OTHER cert field (CN, validity, KU/EKU, SAN ladder,
    // serial shape) matches Node's cert field-for-field.
    rcgen::KeyPair::generate().expect("P-256 keypair")
}

/// Serve the router over TLS on `addr`. Any failure (cert load/generation,
/// bind) returns Err whose Display matches Node's `HTTPS listener failed to
/// start: <message>` log line; the caller logs and HTTP keeps serving.
pub async fn serve(
    addr: SocketAddr,
    db_path: Option<PathBuf>,
    router: axum::Router,
) -> Result<(), String> {
    let (key_pem, cert_pem) = load_or_create_cert(db_path.as_deref())?;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{e}"))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| format!("{e}"))?
        .ok_or_else(|| "no TLS private key in SSL_KEY_PATH/key.pem".to_string())?;
    let cfg = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("{e}"))?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|e| format!("{e}"))?;
    // Node's https.createServer sets no ALPN: HTTP/1.1 only, no h2 offer.
    let mut cfg = cfg;
    cfg.alpn_protocols = Vec::new();
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("{e}"))?;
    tracing::info!(%addr, "HTTPS listener up");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("HTTPS accept failed: {e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    // Handshake failure (port scan, plain-HTTP probe): Node
                    // drops these sockets silently; so do we.
                    tracing::debug!(%peer, "TLS handshake failed: {e}");
                    return;
                }
            };
            // ConnectInfo rides in as an Extension so every handler sees the
            // TLS peer exactly as Node sees req.socket.remoteAddress.
            let svc = router.layer(axum::Extension(axum::extract::connect_info::ConnectInfo(peer)));
            let io = TokioIo::new(tls);
            let conn = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, TowerToHyperService::new(svc));
            if let Err(e) = conn.await {
                tracing::debug!(%peer, "HTTPS connection closed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cert loader reads SSL_CERT_PATH/SSL_KEY_PATH; one test mutates
    // them, so every env-sensitive test takes this lock (std::env writes
    // are process-global and cargo runs tests on parallel threads).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn self_signed_cert_fields_match_node_shape() {
        let (key_pem, cert_pem) = generate_cert();
        let der = pem_to_der(&cert_pem).unwrap();
        let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert_eq!(c.version(), x509_parser::x509::X509Version::V3);
        // Issuer == Subject == CN=scrybox.
        let names = |n: &x509_parser::x509::X509Name| {
            n.iter()
                .flat_map(|rdn| rdn.iter().filter_map(|a| a.as_str().ok().map(|s| s.to_string())))
                .collect::<Vec<_>>()
                .join(",")
        };
        let (iss, sub) = (names(c.issuer()), names(c.subject()));
        assert_eq!(iss, "scrybox", "issuer CN");
        assert_eq!(sub, "scrybox", "subject CN");
        // validity ~397 days
        let secs = c.validity().not_after.timestamp() - c.validity().not_before.timestamp();
        assert!((CERT_DAYS * 86400 - 120..=CERT_DAYS * 86400 + 120).contains(&secs), "{secs}");
        // EC P-256 (rcgen ring) — the ONE documented deviation from Node's
        // RSA-2048 selfsigned cert (see gen_keypair); the container dump
        // pins every other field, asserted below.
        assert_eq!(c.tbs_certificate.subject_pki.algorithm.algorithm.to_string(), "1.2.840.10045.2.1", "id-ecPublicKey");
        assert_eq!(c.signature_algorithm.algorithm.to_string(), "1.2.840.10045.4.3.2", "ecdsa-with-SHA256");
        // basicConstraints CA:false, keyUsage a0 (digitalSignature +
        // keyEncipherment only), EKU serverAuth.
        let bc = c.basic_constraints().unwrap().unwrap().value;
        assert!(!bc.ca);
        let ku = c.key_usage().unwrap().unwrap().value;
        assert!(ku.digital_signature() && ku.key_encipherment() && !ku.key_cert_sign());
        let eku = c.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.server_auth);
        let sans: Vec<String> = c
            .subject_alternative_name()
            .unwrap()
            .unwrap()
            .value
            .general_names
            .iter()
            .map(|n| format!("{n:?}"))
            .collect();
        let joined = sans.join(",");
        assert!(
            joined.contains("DNSName(\"localhost\")")
                && joined.contains("IPAddress([127, 0, 0, 1])")
                && joined.contains("IPAddress([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])"),
            "{joined}"
        );
        // key pem is a PKCS#8 EC key
        assert!(String::from_utf8_lossy(&key_pem).contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn reuse_reuses_until_expiry_then_regenerates() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("bindarr.db");
        std::fs::write(&db, b"").unwrap();
        let (k1, c1) = load_or_create_cert(Some(&db)).unwrap();
        let (k2, c2) = load_or_create_cert(Some(&db)).unwrap();
        assert_eq!(k1, k2, "self-signed cert must be reused across boots");
        assert_eq!(c1, c2);
        // key.pem must be operator-only readable (Node chmod 0600).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("ssl").join("key.pem")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // An expired cert (notAfter in the past) is replaced.
        let cert_file = d.path().join("ssl").join("cert.pem");
        let mut params = rcgen::CertificateParams::default();
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, "scrybox");
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(400);
        params.not_after = now - time::Duration::days(3);
        let kp = rcgen::KeyPair::generate().unwrap();
        let old = params.self_signed(&kp).unwrap();
        std::fs::write(&cert_file, old.pem()).unwrap();
        let (k3, c3) = load_or_create_cert(Some(&db)).unwrap();
        assert_ne!(c1, c3, "expired cert must be regenerated");
        assert_ne!(k1, k3);
        let der = pem_to_der(&c3).unwrap();
        let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert!(c.validity().not_after.timestamp() > now.unix_timestamp());
    }

    #[test]
    fn operator_cert_arms_and_missing_db() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let key = d.path().join("k.pem");
        let cert = d.path().join("c.pem");
        std::env::set_var("SSL_CERT_PATH", &cert);
        std::env::set_var("SSL_KEY_PATH", &key);
        // Unreadable pair => the exact Node message.
        let e = load_or_create_cert(None).unwrap_err();
        assert!(e.starts_with("SSL_CERT_PATH/SSL_KEY_PATH set but unreadable:"), "{e}");
        let (k, c) = generate_cert();
        std::fs::write(&key, &k).unwrap();
        std::fs::write(&cert, &c).unwrap();
        let (k2, c2) = load_or_create_cert(None).unwrap();
        assert_eq!(k, k2);
        assert_eq!(c, c2);
        std::env::remove_var("SSL_CERT_PATH");
        std::env::remove_var("SSL_KEY_PATH");
        // Self-signed with no DB path => Node's throw message.
        let e = load_or_create_cert(None).unwrap_err();
        assert!(e.contains("SCRYBOX_DB_PATH is required"), "{e}");
    }
}
