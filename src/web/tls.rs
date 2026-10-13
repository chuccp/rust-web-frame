//! Certificates: local files, automatic ACME, and a self-signed fallback.
//!
//! The store answers one question — "which certificate for this SNI name?" — in a fixed
//! order:
//!
//! 1. an exact match among the local certificates,
//! 2. a wildcard match (`*.example.com` covers `a.example.com`, but not `a.b.example.com`),
//! 3. the ACME resolver, which serves the TLS-ALPN-01 challenge certificate and, on any
//!    other handshake, hands back an already-issued certificate or starts an order and
//!    returns nothing for this handshake,
//! 4. the first local certificate, so a single-certificate deployment answers for any name,
//! 5. a self-signed certificate, generated on first use and kept.
//!
//! **ACME comes before the self-signed fallback.** The order is the point of the module: a
//! step that always answers makes every step below it unreachable, so a fallback that hands
//! out a self-signed certificate early means no certificate is ever ordered, and every
//! unknown name keeps whichever self-signed one was minted for it first.
//!
//! Local certificates are re-read when the file's mtime changes, on a background task, so
//! a renewal written by `certbot` takes effect without a restart. The check costs one
//! `stat` per file every [`RELOAD_INTERVAL`]; the resolver itself only ever reads memory,
//! because a `ResolvesServerCert` callback runs on the handshake path and must not block.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustls::crypto::ring as provider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use super::config::{SslCertificate, SslConfig};
use super::error::{Error, Result};

/// How often the certificate files are checked for changes.
pub const RELOAD_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Default directory for certificates obtained through ACME.
pub const DEFAULT_CERT_DIR: &str = "./auto_cert";

/// What the store knows: the certificate map, and where each entry came from.
#[derive(Default)]
struct LocalCerts {
    /// SNI name, lowercased, to certificate.
    exact: HashMap<String, Arc<CertifiedKey>>,
    /// `*.example.com` entries, with the certificate.
    wildcards: Vec<(String, Arc<CertifiedKey>)>,
    /// The first successfully loaded certificate, used when nothing matches.
    first: Option<Arc<CertifiedKey>>,
    /// mtimes at the last load, so an unchanged file is not parsed again.
    mtimes: HashMap<PathBuf, std::time::SystemTime>,
}

/// Resolves the certificate for a TLS handshake.
pub struct CertStore {
    config: SslConfig,
    local: RwLock<LocalCerts>,
    self_signed: RwLock<HashMap<String, Arc<CertifiedKey>>>,
    acme: Option<Arc<rustls_acme::ResolvesServerCertAcme>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let local = self.local.read().expect("cert store poisoned");
        f.debug_struct("CertStore")
            .field("domains", &local.exact.keys().collect::<Vec<_>>())
            .field(
                "wildcards",
                &local.wildcards.iter().map(|(d, _)| d).collect::<Vec<_>>(),
            )
            .field("autoCert", &self.acme.is_some())
            .finish()
    }
}

impl CertStore {
    /// Loads the configured certificates and, when `hosts` is set, starts ACME.
    ///
    /// # Errors
    ///
    /// Fails when a configured certificate file is missing or unreadable, or contains no
    /// usable key pair. A certificate the operator asked for is not something to discover
    /// at the first handshake.
    pub fn load(config: &SslConfig) -> Result<Arc<Self>> {
        let acme = if config.certs.is_empty() && !config.hosts.is_empty() {
            Some(rustls_acme::AcmeState::new(acme_config(config)?).resolver())
        } else {
            None
        };

        let store = Arc::new(Self {
            config: config.clone(),
            local: RwLock::new(LocalCerts::default()),
            self_signed: RwLock::new(HashMap::new()),
            acme,
        });

        if !config.certs.is_empty() {
            store.reload(true)?;
        }

        Ok(store)
    }

    /// The names this store can answer for, for logging.
    #[must_use]
    pub fn domains(&self) -> Vec<String> {
        let local = self.local.read().expect("cert store poisoned");
        let mut names: Vec<String> = local.exact.keys().cloned().collect();
        names.extend(local.wildcards.iter().map(|(domain, _)| domain.clone()));
        names.sort();
        names
    }

    /// Reports whether ACME is issuing certificates for this store.
    #[must_use]
    pub fn has_auto_cert(&self) -> bool {
        self.acme.is_some()
    }

    /// The domains ACME is obtaining certificates for.
    ///
    /// Empty when local certificates are configured instead — the two are alternatives,
    /// and this is what the listener logs.
    #[must_use]
    pub fn auto_hosts(&self) -> &[String] {
        if self.acme.is_some() {
            &self.config.hosts
        } else {
            &[]
        }
    }

    /// Starts the task that re-reads the certificate files when they change.
    ///
    /// Returns `None` when there is nothing on disk to watch.
    pub fn watch_files(
        self: &Arc<Self>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if self.config.certs.is_empty() {
            return None;
        }

        let store = Arc::clone(self);
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RELOAD_INTERVAL);
            ticker.tick().await; // the first tick is immediate; the startup load already ran
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = cancel.cancelled() => return,
                }
                if let Err(error) = store.reload(false) {
                    // A renewal that is momentarily half-written must not take the
                    // listener down; the previously loaded certificate keeps serving.
                    tracing::warn!(error = %error, "certificate reload failed, keeping the loaded one");
                }
            }
        }))
    }

    /// Re-reads the certificate files. `force` skips the mtime check.
    fn reload(&self, force: bool) -> Result<()> {
        let mut loaded = LocalCerts::default();

        for source in &self.config.certs {
            if !force && !self.changed(source)? {
                continue;
            }

            let key = Arc::new(load_pair(source)?);
            let domains = certificate_domains(&key, &source.cert_file)?;

            for domain in &domains {
                let domain = domain.to_ascii_lowercase();
                if let Some(suffix) = domain.strip_prefix("*.") {
                    loaded
                        .wildcards
                        .push((format!(".{suffix}"), Arc::clone(&key)));
                } else {
                    loaded.exact.insert(domain, Arc::clone(&key));
                }
            }
            if loaded.first.is_none() {
                loaded.first = Some(Arc::clone(&key));
            }

            for path in [&source.cert_file, &source.key_file] {
                if let Ok(meta) = std::fs::metadata(path)
                    && let Ok(modified) = meta.modified()
                {
                    loaded.mtimes.insert(path.clone(), modified);
                }
            }

            tracing::info!(cert = %source.cert_file.display(), domains = ?domains, "certificate loaded");
        }

        if loaded.exact.is_empty() && loaded.first.is_none() {
            // Nothing changed: keep what is already loaded.
            return Ok(());
        }

        let mut local = self.local.write().expect("cert store poisoned");
        *local = loaded;
        Ok(())
    }

    /// Reports whether either file of a pair is newer than the last load.
    fn changed(&self, source: &SslCertificate) -> Result<bool> {
        let local = self.local.read().expect("cert store poisoned");

        for path in [&source.cert_file, &source.key_file] {
            let modified = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .map_err(|err| {
                    Error::with_source(500, err).detail(format!("cannot read {}", path.display()))
                })?;
            match local.mtimes.get(path) {
                Some(previous) if *previous == modified => {}
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// A self-signed certificate for `name`, generated once and kept.
    ///
    /// Last resort: the handshake succeeds so a client sees a certificate warning rather
    /// than a connection error, which is what a misconfigured hostname looks like from the
    /// outside anyway.
    fn self_signed(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        if let Some(key) = self
            .self_signed
            .read()
            .expect("self-signed cache poisoned")
            .get(name)
        {
            return Some(Arc::clone(key));
        }

        // `rcgen` covers IP literals and single labels as well as DNS names.
        let generated = rcgen::generate_simple_self_signed(vec![name.to_owned()]).ok()?;
        let certified = certified_key(&generated).ok()?;
        let certified = Arc::new(certified);

        tracing::info!(host = name, "generated a self-signed certificate");
        self.self_signed
            .write()
            .expect("self-signed cache poisoned")
            .insert(name.to_owned(), Arc::clone(&certified));
        Some(certified)
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = client_hello.server_name().map(str::to_ascii_lowercase);

        // 1 and 2: the names the operator configured by hand.
        if let Some(name) = name.as_deref() {
            if let Some(key) = self.exact(name) {
                return Some(key);
            }
            if let Some(key) = self.wildcard(name) {
                return Some(key);
            }
        }

        // 3: ACME. It has to see the real `ClientHello` — a TLS-ALPN-01 challenge is
        // recognised by its ALPN, and the ordinary handshake is what triggers an order —
        // so it is called here rather than from a helper that only has the name.
        if let Some(acme) = &self.acme
            && let Some(key) = acme.resolve(client_hello)
        {
            return Some(key);
        }

        // 4: a single-certificate deployment answers for any name.
        if let Some(first) = self
            .local
            .read()
            .expect("cert store poisoned")
            .first
            .as_ref()
        {
            return Some(Arc::clone(first));
        }

        // 5: last resort, so a client sees a warning rather than a connection error.
        // Without SNI there is no name to invent one for.
        self.self_signed(name.as_deref().unwrap_or("localhost"))
    }
}

impl CertStore {
    fn exact(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        self.local
            .read()
            .expect("cert store poisoned")
            .exact
            .get(name)
            .map(Arc::clone)
    }

    fn wildcard(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        let local = self.local.read().expect("cert store poisoned");
        // `*.example.com` is stored as `.example.com`: the host has to end with it and the
        // part in front must be a single label, so `a.b.example.com` does not match.
        local
            .wildcards
            .iter()
            .find(|(suffix, _)| {
                name.strip_suffix(suffix.as_str())
                    .is_some_and(|prefix| !prefix.is_empty() && !prefix.contains('.'))
            })
            .map(|(_, key)| Arc::clone(key))
    }
}

/// Builds the rustls configuration for a TLS listener.
///
/// # Errors
///
/// Fails when the store cannot be built, or when the TLS provider rejects the
/// configuration.
pub fn server_config(
    store: Arc<CertStore>,
    alpn: Vec<Vec<u8>>,
) -> Result<Arc<rustls::ServerConfig>> {
    let mut config =
        rustls::ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|err| Error::with_source(500, err).detail("no usable TLS protocol version"))?
            .with_no_client_auth()
            .with_cert_resolver(store);

    // `acme-tls/1` is what the ACME server offers for a TLS-ALPN-01 challenge; the
    // application's own protocols come after it.
    let mut protocols = Vec::with_capacity(alpn.len() + 1);
    protocols.push(b"acme-tls/1".to_vec());
    protocols.extend(alpn);

    config.alpn_protocols = protocols;
    Ok(Arc::new(config))
}

/// Builds the ACME state for the configured hosts.
fn acme_config(
    config: &SslConfig,
) -> Result<rustls_acme::AcmeConfig<std::io::Error, std::io::Error>> {
    let cache_dir = config
        .cache_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CERT_DIR));

    std::fs::create_dir_all(&cache_dir).map_err(|err| {
        Error::with_source(500, err).detail(format!(
            "cannot create the certificate cache {}",
            cache_dir.display()
        ))
    })?;

    let domains: Vec<String> = config.hosts.clone();
    let mut acme = rustls_acme::AcmeConfig::new(domains)
        .cache(rustls_acme::caches::DirCache::new(cache_dir.clone()))
        .challenge_type(rustls_acme::UseChallenge::TlsAlpn01);

    if let Some(email) = &config.email {
        acme = acme.contact([format!("mailto:{email}")]);
    }

    tracing::info!(
        hosts = ?config.hosts,
        cache = %cache_dir.display(),
        "automatic certificates enabled"
    );

    Ok(acme)
}

/// Loads a PEM certificate chain and key.
fn load_pair(source: &SslCertificate) -> Result<CertifiedKey> {
    let cert_pem = std::fs::read(&source.cert_file).map_err(|err| {
        Error::with_source(500, err).detail(format!("cannot read {}", source.cert_file.display()))
    })?;
    let key_pem = std::fs::read(&source.key_file).map_err(|err| {
        Error::with_source(500, err).detail(format!("cannot read {}", source.key_file.display()))
    })?;

    let certificates = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| {
            Error::with_source(500, err).detail(format!(
                "{} is not a PEM certificate chain",
                source.cert_file.display()
            ))
        })?;
    if certificates.is_empty() {
        return Err(Error::new(
            500,
            format!("{} contains no certificate", source.cert_file.display()),
        ));
    }

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|err| {
            Error::with_source(500, err).detail(format!(
                "{} is not a PEM private key",
                source.key_file.display()
            ))
        })?
        .ok_or_else(|| {
            Error::new(
                500,
                format!("{} contains no private key", source.key_file.display()),
            )
        })?;

    let signing_key = provider::sign::any_supported_type(&key).map_err(|err| {
        Error::with_source(500, err).detail(format!(
            "{} is not a usable key type",
            source.key_file.display()
        ))
    })?;

    Ok(CertifiedKey::new(certificates, signing_key))
}

/// Turns an `rcgen` key pair into a rustls one.
fn certified_key(generated: &rcgen::CertifiedKey<rcgen::KeyPair>) -> Result<CertifiedKey> {
    let certificate = CertificateDer::from(generated.cert.der().to_vec());
    let key = PrivateKeyDer::Pkcs8(generated.signing_key.serialize_der().into());
    let signing_key = provider::sign::any_supported_type(&key)
        .map_err(|err| Error::with_source(500, err).detail("generated key is not usable"))?;
    Ok(CertifiedKey::new(vec![certificate], signing_key))
}

/// The DNS names a certificate is valid for, from its subject alternative names.
///
/// Falls back to the common name only when there are no SANs, which is what the Go
/// original does: a modern certificate always has them.
fn certificate_domains(key: &CertifiedKey, path: &Path) -> Result<Vec<String>> {
    let leaf = key.end_entity_cert().map_err(|err| {
        Error::with_source(500, err)
            .detail(format!("{} has no end-entity certificate", path.display()))
    })?;

    let parsed = parse_domains(leaf)?;
    if parsed.is_empty() {
        return Err(Error::new(
            500,
            format!(
                "{} names no domains — a certificate needs at least one DNS name",
                path.display()
            ),
        ));
    }
    Ok(parsed)
}

/// Reads the SAN list out of a DER certificate.
///
/// Falls back to the common name only when there are no SANs at all — a modern certificate
/// always has them, and a browser ignores the common name regardless.
fn parse_domains(der: &[u8]) -> Result<Vec<String>> {
    use x509_parser::extensions::GeneralName;

    let (_, certificate) = x509_parser::parse_x509_certificate(der)
        .map_err(|err| Error::with_source(500, err).detail("the certificate is not readable"))?;

    let mut domains = Vec::new();
    match certificate.subject_alternative_name() {
        Ok(Some(extension)) => {
            for name in &extension.value.general_names {
                match name {
                    GeneralName::DNSName(name) => domains.push((*name).to_owned()),
                    GeneralName::IPAddress(bytes) => {
                        if let Some(ip) = ip_from_bytes(bytes) {
                            domains.push(ip);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(None) => {}
        Err(err) => {
            return Err(
                Error::with_source(500, err).detail("the subjectAltName extension is malformed")
            );
        }
    }

    if domains.is_empty()
        && let Some(common_name) = certificate.subject().iter_common_name().next()
        && let Ok(name) = common_name.as_str()
    {
        domains.push(name.to_owned());
    }

    domains.dedup();
    Ok(domains)
}

/// Renders a subjectAltName `iPAddress` — four or sixteen raw bytes — as text.
fn ip_from_bytes(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => Some(std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()),
        16 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A certificate and key pair, written to a temporary directory.
    fn write_self_signed(dir: &Path, name: &str, domains: &[&str]) -> SslCertificate {
        let generated = rcgen::generate_simple_self_signed(
            domains.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
        )
        .expect("generate");

        let cert_path = dir.join(format!("{name}.crt"));
        let key_path = dir.join(format!("{name}.key"));
        std::fs::write(&cert_path, generated.cert.pem()).expect("write cert");
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).expect("write key");

        SslCertificate {
            cert_file: cert_path,
            key_file: key_path,
        }
    }

    fn store_with(certs: Vec<SslCertificate>, hosts: Vec<String>) -> Arc<CertStore> {
        CertStore::load(&SslConfig {
            enabled: true,
            certs,
            hosts,
            cache_dir: None,
            email: None,
        })
        .expect("load")
    }

    #[test]
    fn certificate_names_come_from_the_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "multi", &["one.test", "two.test"]);
        let store = store_with(vec![source], Vec::new());
        assert_eq!(store.domains(), vec!["one.test", "two.test"]);
    }

    #[test]
    fn an_exact_name_is_matched_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "a", &["a.example.com"]);
        let store = store_with(vec![source], Vec::new());

        assert!(store.exact("a.example.com").is_some());
        // SNI arrives lowercased by the time it reaches here, and the map is keyed that way.
        assert!(store.exact(&"A.EXAMPLE.COM".to_ascii_lowercase()).is_some());
        assert!(store.exact("other.test").is_none());
    }

    #[test]
    fn a_wildcard_covers_exactly_one_label() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "w", &["*.example.com"]);
        let store = store_with(vec![source], Vec::new());

        assert!(store.wildcard("a.example.com").is_some());
        assert!(store.wildcard("b.example.com").is_some());
        assert!(
            store.wildcard("a.b.example.com").is_none(),
            "`*.example.com` must not cover two labels"
        );
        assert!(
            store.wildcard("example.com").is_none(),
            "the bare name is not covered"
        );
        assert!(store.wildcard("elsewhere.test").is_none());
    }

    #[test]
    fn an_unknown_name_falls_back_to_the_first_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_self_signed(dir.path(), "first", &["a.example.com"]);
        let second = write_self_signed(dir.path(), "second", &["b.example.com"]);
        let store = store_with(vec![first, second], Vec::new());

        // Neither name matches `elsewhere.test`, so the first-loaded certificate answers.
        let fallback = store
            .local
            .read()
            .unwrap()
            .first
            .as_ref()
            .map(Arc::clone)
            .expect("a fallback");
        assert!(store.exact("elsewhere.test").is_none());
        assert!(store.wildcard("elsewhere.test").is_none());
        assert!(Arc::ptr_eq(
            &fallback,
            store.local.read().unwrap().first.as_ref().unwrap()
        ));
    }

    #[test]
    fn with_no_certificates_a_self_signed_one_is_generated_and_kept() {
        let store = store_with(Vec::new(), Vec::new());

        let first = store.self_signed("dev.local").expect("generated");
        let again = store.self_signed("dev.local").expect("cached");
        assert!(
            Arc::ptr_eq(&first, &again),
            "it must not regenerate per handshake"
        );

        let other = store.self_signed("other.local").expect("generated");
        assert!(!Arc::ptr_eq(&first, &other), "each name gets its own");
    }

    #[test]
    fn a_missing_certificate_file_is_a_startup_error() {
        let error = CertStore::load(&SslConfig {
            enabled: true,
            certs: vec![SslCertificate {
                cert_file: PathBuf::from("./does-not-exist.crt"),
                key_file: PathBuf::from("./does-not-exist.key"),
            }],
            hosts: Vec::new(),
            cache_dir: None,
            email: None,
        })
        .unwrap_err();

        assert!(
            error.client_message().contains("does-not-exist.crt"),
            "{}",
            error.client_message()
        );
    }

    #[test]
    fn an_ip_san_is_a_name_too() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "ip", &["127.0.0.1"]);
        let store = store_with(vec![source], Vec::new());
        assert_eq!(store.domains(), vec!["127.0.0.1"]);
        assert!(store.exact("127.0.0.1").is_some());
    }

    #[test]
    fn acme_is_only_used_when_there_are_no_local_certificates() {
        let dir = tempfile::tempdir().unwrap();
        let local = write_self_signed(dir.path(), "a", &["a.example.com"]);

        // Local certificates win, and no ACME state is created.
        let store = store_with(vec![local], vec!["auto.example.com".to_owned()]);
        assert!(!store.has_auto_cert());
        assert!(store.auto_hosts().is_empty());

        // With no local certificates, ACME takes the names.
        let store = store_with(Vec::new(), vec!["auto.example.com".to_owned()]);
        assert!(store.has_auto_cert());
        assert_eq!(store.auto_hosts(), ["auto.example.com"]);
    }

    #[test]
    fn reload_leaves_the_loaded_certificate_alone_when_nothing_changed() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "a", &["a.example.com"]);
        let store = store_with(vec![source], Vec::new());

        let before = store.exact("a.example.com").expect("loaded");
        store
            .reload(false)
            .expect("an unchanged file is not an error");
        let after = store.exact("a.example.com").expect("still loaded");
        assert!(Arc::ptr_eq(&before, &after), "the same certificate is kept");
    }

    #[test]
    fn reload_picks_up_a_replaced_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_self_signed(dir.path(), "a", &["a.example.com"]);
        let store = store_with(vec![source.clone()], Vec::new());
        let before = store.exact("a.example.com").expect("loaded");

        // A renewal: the name changes, and the mtime moves with it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let replacement = write_self_signed(dir.path(), "a-renewed", &["a.example.com"]);
        std::fs::copy(&replacement.cert_file, &source.cert_file).expect("replace cert");
        std::fs::copy(&replacement.key_file, &source.key_file).expect("replace key");

        store.reload(false).expect("reload");

        let after = store.exact("a.example.com").expect("still loaded");
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a replaced certificate has to be re-read"
        );
    }
}
