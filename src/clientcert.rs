//! The remote client's TLS client certificate: a second factor beside the bearer token.
//!
//! `[server].client_cert` / `client_key` (or `DM_CLIENT_CERT` / `DM_CLIENT_KEY`) name two PEM
//! files. `RemoteClient::new` loads them once into a `reqwest::Identity`, so every remote call
//! (CLI, MCP, hooks, `dmem status`) presents the certificate. `dmem upgrade` and `dmem migrate
//! --url` build their own clients and never see it.
//!
//! The key file is checked before it is read: no symlink, no access for group or others, owned
//! by the current user (Unix). Its bytes are held in a wiped buffer, never logged, never put in an
//! error message (errors name the path). Copies that rustls and reqwest make internally are not
//! ours to wipe.

use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::io::Read;
use std::path::Path;
use zeroize::Zeroizing;

/// A certificate or key file is a few KiB; anything bigger is not one.
const MAX_FILE: u64 = 64 * 1024;
/// A certificate this close to its end is reported as a warning by `dmem doctor`.
pub const WARN_DAYS: i64 = 30;

/// Both paths or neither. A half-configured pair is an error, never a silent token-only fallback.
pub fn pair<'a>(cert: Option<&'a str>, key: Option<&'a str>) -> Result<Option<(&'a str, &'a str)>> {
    let blank = |s: &&str| !s.trim().is_empty();
    match (cert.filter(blank), key.filter(blank)) {
        (None, None) => Ok(None),
        (Some(c), Some(k)) => Ok(Some((c, k))),
        (Some(_), None) => bail!("client_cert is set but client_key is not (set [server].client_key or DM_CLIENT_KEY)"),
        (None, Some(_)) => bail!("client_key is set but client_cert is not (set [server].client_cert or DM_CLIENT_CERT)"),
    }
}

/// What `dmem doctor` shows about a certificate. Nothing here comes from the key.
#[derive(Debug, Clone, PartialEq)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    /// Serial number as hex.
    pub serial: String,
    /// Seconds since the Unix epoch.
    pub not_before: i64,
    pub not_after: i64,
    /// `Some(false)` when the certificate has an extended key usage that excludes clientAuth;
    /// `None` when it has none (unrestricted) or it could not be read.
    pub client_auth: Option<bool>,
}

pub struct Loaded {
    pub identity: reqwest::Identity,
    pub info: CertInfo,
}

/// The names in a certificate are whatever its issuer wrote; `dmem doctor` prints them, so a
/// control character (a terminal escape sequence, a newline) is shown as `?`.
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// Read a certificate's public fields from its DER.
pub fn inspect(der: &[u8]) -> Result<CertInfo> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).map_err(|e| anyhow!("not an X.509 certificate: {e}"))?;
    let client_auth = match cert.extended_key_usage() {
        Ok(Some(eku)) => Some(eku.value.client_auth || eku.value.any),
        _ => None,
    };
    Ok(CertInfo {
        subject: printable(&cert.subject().to_string()),
        issuer: printable(&cert.issuer().to_string()),
        serial: cert.raw_serial().iter().map(|b| format!("{b:02x}")).collect(),
        not_before: cert.validity().not_before.timestamp(),
        not_after: cert.validity().not_after.timestamp(),
        client_auth,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Validity {
    /// Whole days left, more than `WARN_DAYS`.
    Ok(i64),
    /// Whole days left, `WARN_DAYS` or fewer.
    Soon(i64),
    /// Whole days since it ended.
    Expired(i64),
    /// Whole days until it starts.
    NotYet(i64),
}

pub fn validity(info: &CertInfo, now: i64) -> Validity {
    let day = 86_400;
    if now < info.not_before {
        Validity::NotYet((info.not_before - now) / day)
    } else if now > info.not_after {
        Validity::Expired((now - info.not_after) / day)
    } else {
        let left = (info.not_after - now) / day;
        if left <= WARN_DAYS { Validity::Soon(left) } else { Validity::Ok(left) }
    }
}

/// `YYYY-MM-DD` (UTC) of a Unix timestamp.
pub fn ymd(ts: i64) -> String {
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Open the key file and read it into a wiped buffer, after the file checks.
fn read_key(path: &str) -> Result<Zeroizing<Vec<u8>>> {
    let p = Path::new(path);
    let lstat = std::fs::symlink_metadata(p).map_err(|e| anyhow!("client_key {path}: {e}"))?;
    if lstat.file_type().is_symlink() {
        bail!("client_key {path} is a symlink; point the config at the key file itself");
    }
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: a link swapped in after the check above is refused too.
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(p)
    }
    .map_err(|e| anyhow!("client_key {path}: {e}"))?;
    #[cfg(not(unix))]
    let mut file = std::fs::File::open(p).map_err(|e| anyhow!("client_key {path}: {e}"))?;
    // Every check below is on the open file, so it is the file that is read.
    let meta = file.metadata().map_err(|e| anyhow!("client_key {path}: {e}"))?;
    if !meta.is_file() {
        bail!("client_key {path} is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!("client_key {path} is accessible by group or others (mode {mode:03o}); run: chmod 600 {path}");
        }
        // SAFETY: geteuid has no preconditions and cannot fail.
        if meta.uid() != unsafe { libc::geteuid() } {
            bail!("client_key {path} is owned by another user; it must belong to the user running dmem");
        }
    }
    let mut buf = Zeroizing::new(Vec::new());
    (&mut file).take(MAX_FILE + 1).read_to_end(&mut buf).map_err(|e| anyhow!("client_key {path}: {e}"))?;
    if buf.len() as u64 > MAX_FILE {
        bail!("client_key {path} is too large to be a private key");
    }
    Ok(buf)
}

fn read_cert(path: &str) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path).map_err(|e| anyhow!("client_cert {path}: {e}"))?;
    if !meta.is_file() {
        bail!("client_cert {path} is not a regular file");
    }
    if meta.len() > MAX_FILE {
        bail!("client_cert {path} is too large to be a certificate");
    }
    std::fs::read(path).map_err(|e| anyhow!("client_cert {path}: {e}"))
}

/// Does this PEM hold a passphrase-protected key? rustls cannot decrypt one, and a prompt would
/// hang a hook, so it is refused.
fn is_encrypted(pem: &[u8]) -> bool {
    let has = |needle: &[u8]| pem.windows(needle.len()).any(|w| w == needle);
    has(b"BEGIN ENCRYPTED PRIVATE KEY") || has(b"Proc-Type: 4,ENCRYPTED")
}

/// Load the certificate and key: file checks, parse, check they belong together, build the
/// identity. Every error names a path and says what to do; none carries key material.
pub fn load(cert_path: &str, key_path: &str) -> Result<Loaded> {
    let cert_pem = read_cert(cert_path)?;
    let key_pem = read_key(key_path)?;
    if is_encrypted(&key_pem) {
        bail!("client_key {key_path} is passphrase-protected; dmem cannot ask for a passphrase (hooks are non-interactive). Remove it: openssl pkey -in <key> -out <new key>");
    }
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow!("client_cert {cert_path} is not a PEM certificate"))?;
    let leaf = chain.first().ok_or_else(|| anyhow!("client_cert {cert_path} holds no certificate"))?;
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|_| anyhow!("client_key {key_path} holds no PKCS#8, SEC1 or PKCS#1 PEM private key"))?;
    // The key must be the certificate's own: a mismatch otherwise shows up only as an opaque
    // handshake failure on the first request.
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let signer = provider
        .key_provider
        .load_private_key(key)
        .map_err(|_| anyhow!("client_key {key_path} is not a private key this build can use"))?;
    rustls::sign::CertifiedKey::new(chain.clone(), signer)
        .keys_match()
        .map_err(|_| anyhow!("client_key {key_path} does not match client_cert {cert_path}"))?;
    let info = inspect(leaf).with_context(|| format!("client_cert {cert_path}"))?;
    let mut both = Zeroizing::new(Vec::with_capacity(cert_pem.len() + key_pem.len() + 1));
    both.extend_from_slice(&cert_pem);
    both.push(b'\n');
    both.extend_from_slice(&key_pem);
    let identity = reqwest::Identity::from_pem(&both).map_err(|_| anyhow!("client_key {key_path} and client_cert {cert_path} could not be loaded as a TLS identity"))?;
    Ok(Loaded { identity, info })
}

/// The `dmem doctor` view of the configured client certificate.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `none`, `ok`, `warn` or `error`.
    pub state: &'static str,
    pub line: String,
}

pub fn report(cert: Option<&str>, key: Option<&str>, now: i64) -> Report {
    let (c, k) = match pair(cert, key) {
        Ok(None) => return Report { state: "none", line: "none (token only)".into() },
        Ok(Some(p)) => p,
        Err(e) => return Report { state: "error", line: e.to_string() },
    };
    let loaded = match load(c, k) {
        Ok(l) => l,
        Err(e) => return Report { state: "error", line: format!("{e:#}") },
    };
    let i = &loaded.info;
    let mut line = format!("{}, issuer {}, serial {}, expires {}", i.subject, i.issuer, i.serial, ymd(i.not_after));
    let mut state = "ok";
    match validity(i, now) {
        Validity::Ok(d) => line.push_str(&format!(" ({d} days)")),
        Validity::Soon(d) => {
            state = "warn";
            line.push_str(&format!(" (WARNING: {d} days left)"));
        }
        Validity::Expired(d) => {
            state = "error";
            line.push_str(&format!(" (EXPIRED {d} days ago)"));
        }
        Validity::NotYet(d) => {
            state = "error";
            line.push_str(&format!(" (NOT YET VALID: starts in {d} days)"));
        }
    }
    if i.client_auth == Some(false) {
        state = "error";
        line.push_str("; its extended key usage does not include clientAuth, so a server will refuse it");
    }
    Report { state, line }
}

#[cfg(all(test, feature = "server"))]
pub(crate) mod tests {
    use super::*;
    use crate::config::ServerLink;
    use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use rustls::server::WebPkiClientVerifier;
    use std::io::Write;
    use std::sync::Arc;

    /// A throwaway client CA.
    pub(crate) struct Ca {
        cert: rcgen::Certificate,
        key: KeyPair,
    }

    pub(crate) fn ca(name: &str) -> Ca {
        let mut p = CertificateParams::new(vec![]).unwrap();
        p.distinguished_name.push(DnType::CommonName, name);
        p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        Ca { cert: p.self_signed(&key).unwrap(), key }
    }

    /// A client leaf from `ca`: (certificate PEM, key PEM). `years` = (not_before year, not_after year).
    pub(crate) fn leaf(ca: &Ca, cn: &str, ekus: Vec<ExtendedKeyUsagePurpose>, years: (i32, i32)) -> (String, String) {
        let mut p = CertificateParams::new(vec![]).unwrap();
        p.distinguished_name.push(DnType::CommonName, cn);
        p.extended_key_usages = ekus;
        p.not_before = rcgen::date_time_ymd(years.0, 1, 1);
        p.not_after = rcgen::date_time_ymd(years.1, 1, 1);
        let key = KeyPair::generate().unwrap();
        let cert = p.signed_by(&key, &ca.cert, &ca.key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    pub(crate) fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("dmcc-{tag}-{}-{}", std::process::id(), crate::entry::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a file with the given mode (Unix; elsewhere the mode is ignored).
    pub(crate) fn write(path: &Path, text: &str, mode: u32) {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(mode);
        }
        let _ = mode;
        o.open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    /// A local TLS server on 127.0.0.1 that answers `conns` connections with HTTP 200 and
    /// records, per connection, whether the client presented a certificate (`None` = the
    /// handshake failed). With `client_ca`, it asks for a client certificate from that CA:
    /// required when `mandatory`, otherwise optional.
    pub(crate) struct TestServer {
        pub url: String,
        pub ca_pem_path: std::path::PathBuf,
        seen: std::thread::JoinHandle<Vec<Option<bool>>>,
    }

    impl TestServer {
        pub(crate) fn seen(self) -> Vec<Option<bool>> {
            self.seen.join().unwrap()
        }
    }

    pub(crate) fn serve(dir: &Path, client_ca: Option<&Ca>, mandatory: bool, conns: usize) -> TestServer {
        crate::tls::install_provider();
        let sc = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let ca_pem_path = dir.join("server.pem");
        std::fs::write(&ca_pem_path, sc.cert.pem()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().unwrap();
        let builder = match client_ca {
            Some(ca) => {
                let mut roots = rustls::RootCertStore::empty();
                roots.add(ca.cert.der().clone()).unwrap();
                let mut vb = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
                if !mandatory {
                    vb = vb.allow_unauthenticated();
                }
                builder.with_client_cert_verifier(vb.build().unwrap())
            }
            None => builder.with_no_client_auth(),
        };
        let cfg = Arc::new(
            builder
                .with_single_cert(vec![sc.cert.der().clone()], PrivatePkcs8KeyDer::from(sc.key_pair.serialize_der()).into())
                .unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("https://localhost:{}", listener.local_addr().unwrap().port());
        let seen = std::thread::spawn(move || {
            use std::io::Read;
            let mut out = Vec::new();
            for _ in 0..conns {
                let (tcp, _) = listener.accept().unwrap();
                let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg.clone()).unwrap(), tcp);
                let mut buf = [0u8; 4096];
                let mut got = Vec::new();
                let ok = loop {
                    match tls.read(&mut buf) {
                        Ok(0) => break false,
                        Ok(n) => {
                            got.extend_from_slice(&buf[..n]);
                            if got.windows(4).any(|w| w == b"\r\n\r\n") {
                                break true;
                            }
                        }
                        Err(_) => break false,
                    }
                };
                if !ok {
                    out.push(None);
                    continue;
                }
                out.push(Some(tls.conn.peer_certificates().is_some_and(|c| !c.is_empty())));
                let body = "{}";
                let _ = write!(tls, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = tls.flush();
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
            out
        });
        TestServer { url, ca_pem_path, seen }
    }

    fn link(url: &str, ca: &Path, cert: Option<&Path>, key: Option<&Path>) -> ServerLink {
        ServerLink {
            url: url.into(),
            token: "tok".into(),
            insecure: false,
            ca_cert: Some(ca.display().to_string()),
            client_cert: cert.map(|p| p.display().to_string()),
            client_key: key.map(|p| p.display().to_string()),
        }
    }

    fn post(l: &ServerLink) -> Result<(reqwest::StatusCode, String)> {
        crate::client::RemoteClient::new(l)?.send_post_for_test("/recall", &serde_json::json!({}))
    }

    /// Files for a client leaf under `dir`: (cert path, key path).
    fn files(dir: &Path, name: &str, cert: &str, key: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let (c, k) = (dir.join(format!("{name}.pem")), dir.join(format!("{name}.key")));
        write(&c, cert, 0o644);
        write(&k, key, 0o600);
        (c, k)
    }

    const NOW: i64 = 1_800_000_000; // 2027-01-15

    #[test]
    fn pair_is_both_or_neither() {
        assert_eq!(pair(None, None).unwrap(), None);
        assert_eq!(pair(Some(" "), Some("")).unwrap(), None, "blank = unset");
        assert_eq!(pair(Some("/c"), Some("/k")).unwrap(), Some(("/c", "/k")));
        assert!(pair(Some("/c"), None).unwrap_err().to_string().contains("client_key"));
        assert!(pair(None, Some("/k")).unwrap_err().to_string().contains("client_cert"));
    }

    #[test]
    fn ymd_is_the_utc_calendar_date() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951_782_400), "2000-02-29");
        assert_eq!(ymd(1_800_000_000), "2027-01-15");
        assert_eq!(ymd(-86_400), "1969-12-31");
    }

    #[test]
    fn a_certificate_from_the_servers_client_ca_is_presented_and_accepted() {
        let d = tmpdir("ok");
        let ca = ca("test client CA");
        let (c, k) = leaf(&ca, "bukumek", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "bukumek", &c, &k);
        let srv = serve(&d, Some(&ca), true, 1);
        let (status, _) = post(&link(&srv.url, &srv.ca_pem_path, Some(&cp), Some(&kp))).expect("the request succeeds");
        assert_eq!(status.as_u16(), 200);
        assert_eq!(srv.seen(), vec![Some(true)], "the server saw the client certificate");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn without_a_certificate_the_request_fails_with_the_hint() {
        let d = tmpdir("none");
        let ca = ca("test client CA");
        let srv = serve(&d, Some(&ca), true, 1);
        let err = post(&link(&srv.url, &srv.ca_pem_path, None, None)).expect_err("the server requires a certificate");
        let text = format!("{err:#}");
        assert!(text.contains("client certificate") && text.contains("client_cert"), "{text}");
        assert_eq!(srv.seen(), vec![None]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_certificate_from_another_ca_is_refused() {
        let d = tmpdir("other");
        let (trusted, other) = (ca("trusted"), ca("someone else"));
        let (c, k) = leaf(&other, "intruder", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "intruder", &c, &k);
        let srv = serve(&d, Some(&trusted), true, 1);
        let err = post(&link(&srv.url, &srv.ca_pem_path, Some(&cp), Some(&kp))).expect_err("not the server's CA");
        assert!(!err.to_string().is_empty());
        assert_eq!(srv.seen(), vec![None]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_server_that_does_not_ask_still_works_with_a_certificate_configured() {
        let d = tmpdir("noask");
        let ca = ca("test client CA");
        let (c, k) = leaf(&ca, "bukumek", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "bukumek", &c, &k);
        let srv = serve(&d, None, false, 1);
        let (status, _) = post(&link(&srv.url, &srv.ca_pem_path, Some(&cp), Some(&kp))).expect("sending a cert changes nothing");
        assert_eq!(status.as_u16(), 200);
        let _ = srv.seen();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "self-update")]
    #[test]
    fn dmem_upgrade_never_offers_the_client_certificate() {
        let _g = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let d = tmpdir("upg");
        let ca = ca("test client CA");
        let (c, k) = leaf(&ca, "bukumek", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "bukumek", &c, &k);
        std::env::set_var("DM_CLIENT_CERT", &cp);
        std::env::set_var("DM_CLIENT_KEY", &kp);
        // the server asks for a certificate but lets an anonymous client through, and says what it saw
        let srv = serve(&d, Some(&ca), false, 1);
        let pem = std::fs::read(&srv.ca_pem_path).unwrap();
        // upgrade::http() cannot be pinned to the test server's certificate, so check the builder
        // it is made from (tls::http_builder), pinned to the test server
        let b = crate::tls::http_builder().add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap());
        let _ = b.build().unwrap().get(format!("{}/x", srv.url)).send();
        assert_eq!(srv.seen(), vec![Some(false)], "an http_builder() client, as upgrade::http() builds it, sends no certificate");
        std::env::remove_var("DM_CLIENT_CERT");
        std::env::remove_var("DM_CLIENT_KEY");
        // and upgrade::http() itself is that builder plus a user agent and a timeout, nothing about [server]
        assert!(crate::upgrade::http().is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_checks_refuse_a_symlink_a_loose_mode_and_a_mismatch() {
        let d = tmpdir("keys");
        let ca = ca("test client CA");
        let (c, k) = leaf(&ca, "bukumek", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "good", &c, &k);
        assert!(load(cp.to_str().unwrap(), kp.to_str().unwrap()).is_ok(), "the good pair loads");
        let secret: String = k.lines().nth(1).unwrap().chars().take(24).collect();
        let no_leak = |e: &anyhow::Error| {
            let t = format!("{e:#}");
            assert!(!t.contains(&secret), "the error carries key bytes: {t}");
            t
        };
        // a symlink to the real key
        let link_path = d.join("link.key");
        std::os::unix::fs::symlink(&kp, &link_path).unwrap();
        let t = no_leak(&load(cp.to_str().unwrap(), link_path.to_str().unwrap()).err().unwrap());
        assert!(t.contains("symlink") && t.contains("link.key"), "{t}");
        // group-readable
        let loose = d.join("loose.key");
        write(&loose, &k, 0o640);
        let t = no_leak(&load(cp.to_str().unwrap(), loose.to_str().unwrap()).err().unwrap());
        assert!(t.contains("chmod 600") && t.contains("loose.key"), "{t}");
        // a key that is not this certificate's
        let (_, other_key) = leaf(&ca, "other", vec![], (2026, 2028));
        let (_, okp) = files(&d, "other", &c, &other_key);
        let t = no_leak(&load(cp.to_str().unwrap(), okp.to_str().unwrap()).err().unwrap());
        assert!(t.contains("does not match"), "{t}");
        // a passphrase-protected key
        let enc = d.join("enc.key");
        write(&enc, "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIBqjBU\n-----END ENCRYPTED PRIVATE KEY-----\n", 0o600);
        let t = no_leak(&load(cp.to_str().unwrap(), enc.to_str().unwrap()).err().unwrap());
        assert!(t.contains("passphrase"), "{t}");
        // not a key at all
        let junk = d.join("junk.key");
        write(&junk, "hello\n", 0o600);
        assert!(load(cp.to_str().unwrap(), junk.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn doctor_reports_subject_expiry_and_warns_then_errors_as_the_date_nears() {
        let d = tmpdir("doc");
        let ca = ca("dmem client CA");
        // valid 2026-01-01 .. 2028-01-01
        let (c, k) = leaf(&ca, "bukumek", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "bukumek", &c, &k);
        let (cs, ks) = (cp.to_str(), kp.to_str());
        let r = report(cs, ks, NOW);
        assert_eq!(r.state, "ok", "{}", r.line);
        assert!(r.line.contains("CN=bukumek") && r.line.contains("issuer CN=dmem client CA") && r.line.contains("expires 2028-01-01"), "{}", r.line);
        let end = 1_830_297_600; // 2028-01-01T00:00:00Z
        assert_eq!(ymd(end), "2028-01-01");
        let r = report(cs, ks, end - 29 * 86_400);
        assert_eq!((r.state, r.line.contains("WARNING: 29 days")), ("warn", true), "{}", r.line);
        let r = report(cs, ks, end - 31 * 86_400);
        assert_eq!(r.state, "ok", "31 days left is not yet a warning: {}", r.line);
        let r = report(cs, ks, end + 3 * 86_400);
        assert_eq!((r.state, r.line.contains("EXPIRED 3 days ago")), ("error", true), "{}", r.line);
        let r = report(cs, ks, 1_700_000_000); // before 2026-01-01
        assert_eq!((r.state, r.line.contains("NOT YET VALID")), ("error", true), "{}", r.line);
        // never key material in what doctor prints
        let secret: String = k.lines().nth(1).unwrap().chars().take(24).collect();
        assert!(!r.line.contains(&secret));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn doctor_shows_control_characters_in_a_certificate_name_as_question_marks() {
        let d = tmpdir("ctl");
        let ca = ca("dmem client CA");
        let (c, k) = leaf(&ca, "bukumek\u{1b}[2J\nspoof", vec![ExtendedKeyUsagePurpose::ClientAuth], (2026, 2028));
        let (cp, kp) = files(&d, "ctl", &c, &k);
        let r = report(cp.to_str(), kp.to_str(), NOW);
        assert!(!r.line.chars().any(|ch| ch.is_control()), "{:?}", r.line);
        assert!(r.line.contains("bukumek?[2J?spoof"), "{:?}", r.line);
        assert_eq!(printable("a\tb\u{7f}c"), "a?b?c");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn doctor_flags_a_certificate_that_cannot_be_used_for_client_auth_and_the_unset_cases() {
        let d = tmpdir("eku");
        let ca = ca("dmem client CA");
        let (c, k) = leaf(&ca, "server-only", vec![ExtendedKeyUsagePurpose::ServerAuth], (2026, 2028));
        let (cp, kp) = files(&d, "so", &c, &k);
        let r = report(cp.to_str(), kp.to_str(), NOW);
        assert_eq!(r.state, "error");
        assert!(r.line.contains("clientAuth"), "{}", r.line);
        let r = report(None, None, NOW);
        assert_eq!((r.state, r.line.as_str()), ("none", "none (token only)"));
        let r = report(cp.to_str(), None, NOW);
        assert_eq!(r.state, "error");
        assert!(r.line.contains("client_key"), "{}", r.line);
        let _ = std::fs::remove_dir_all(&d);
    }
}
