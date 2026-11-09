//! Local certificate authority behind the ingress TLS certificate.
//!
//! k3dev signs each cluster's wildcard certificate with one root CA kept in
//! `~/.k3dev/ca`. Browsers and CLI tools accept that certificate only when the
//! host trusts the root CA, so every cluster start checks the system trust
//! store and asks for sudo to install the CA when it is missing — a declined
//! or failed prompt is simply asked again on the next start.
//!
//! Certificates are shaped for the strictest common verifiers:
//! - OpenSSL's strict mode (default in Python 3.13+) requires a key usage on
//!   the CA and an authority key identifier on the leaf;
//! - Apple platforms require a serverAuth extended key usage and at most 825
//!   days of validity for TLS certificates issued after mid-2019.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Utc};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::mpsc;
use x509_parser::prelude::*;

use crate::ui::components::OutputLine;

/// Common name of the k3dev root CA, also its label in trust stores.
pub(crate) const CA_COMMON_NAME: &str = "k3dev Local CA";

/// How long a generated root CA is valid.
const CA_VALIDITY_DAYS: i64 = 3650;

/// How long a generated leaf is valid — under Apple's 825-day ceiling.
const LEAF_VALIDITY_DAYS: i64 = 820;

/// Longest leaf validity Apple platforms accept.
const APPLE_MAX_VALIDITY_DAYS: i64 = 825;

/// Certificates are regenerated once they are this close to expiry.
const RENEW_BEFORE_DAYS: i64 = 30;

/// A PEM certificate together with its PEM private key.
#[derive(Clone)]
pub(crate) struct CertKeyPem {
    pub cert: String,
    pub key: String,
}

/// Valid from the start of yesterday (UTC), so a host clock running a little
/// behind still accepts a certificate generated moments ago.
fn set_validity(params: &mut CertificateParams, now: DateTime<Utc>, days: i64) {
    let start = (now - Duration::days(1)).date_naive();
    let end = start + Duration::days(days);
    params.not_before = rcgen::date_time_ymd(start.year(), start.month() as u8, start.day() as u8);
    params.not_after = rcgen::date_time_ymd(end.year(), end.month() as u8, end.day() as u8);
}

/// Generate a new self-signed root CA valid from yesterday.
pub(crate) fn generate_ca(now: DateTime<Utc>) -> Result<CertKeyPem> {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| anyhow!("Failed to create CA params: {}", e))?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, CA_COMMON_NAME);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "k3dev");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    set_validity(&mut params, now, CA_VALIDITY_DAYS);

    let key = KeyPair::generate().map_err(|e| anyhow!("Failed to generate CA key: {}", e))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| anyhow!("Failed to self-sign CA: {}", e))?;
    Ok(CertKeyPem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// DER contents of a single PEM block.
fn pem_to_der(pem_str: &str) -> Result<Vec<u8>, String> {
    ::pem::parse(pem_str)
        .map(|p| p.into_contents())
        .map_err(|e| format!("invalid PEM: {}", e))
}

fn parse_der(der: &[u8]) -> Result<X509Certificate<'_>, String> {
    X509Certificate::from_der(der)
        .map(|(_, cert)| cert)
        .map_err(|e| format!("invalid certificate: {}", e))
}

fn subject_key_id(cert: &X509Certificate<'_>) -> Option<Vec<u8>> {
    cert.extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0.to_vec()),
            _ => None,
        })
}

fn authority_key_id(cert: &X509Certificate<'_>) -> Option<Vec<u8>> {
    cert.extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|id| id.0.to_vec())
            }
            _ => None,
        })
}

fn not_after(cert: &X509Certificate<'_>) -> DateTime<Utc> {
    DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0).unwrap_or_default()
}

/// `Err` when `cert` is not yet valid or expires within [`RENEW_BEFORE_DAYS`].
fn check_validity_window(cert: &X509Certificate<'_>, now: DateTime<Utc>) -> Result<(), String> {
    if now.timestamp() < cert.validity().not_before.timestamp() {
        return Err("not valid yet".to_string());
    }
    let expiry = not_after(cert);
    if expiry - now < Duration::days(RENEW_BEFORE_DAYS) {
        return Err(format!("expires {}", expiry.format("%Y-%m-%d")));
    }
    Ok(())
}

/// Check that a stored CA can keep signing certificates; `Err` says why not.
pub(crate) fn check_ca(ca: &CertKeyPem, now: DateTime<Utc>) -> Result<(), String> {
    let der = pem_to_der(&ca.cert)?;
    let cert = parse_der(&der)?;
    if !cert.is_ca() {
        return Err("not a CA certificate".to_string());
    }
    // Strict verifiers refuse a CA without one (earlier k3dev releases omitted it)
    match cert.key_usage() {
        Ok(Some(ku)) if ku.value.key_cert_sign() => {}
        _ => return Err("CA has no certificate-signing key usage".to_string()),
    }
    if subject_key_id(&cert).is_none() {
        return Err("CA has no subject key identifier".to_string());
    }
    check_validity_window(&cert, now).map_err(|e| format!("CA {}", e))?;
    let key = KeyPair::from_pem(&ca.key).map_err(|e| format!("invalid CA key: {}", e))?;
    if key.public_key_raw() != cert.public_key().subject_public_key.data.as_ref() {
        return Err("CA key does not match the CA certificate".to_string());
    }
    Ok(())
}

/// Generate a leaf certificate for `domain` and `*.domain`, signed by `ca`.
pub(crate) fn generate_leaf(
    ca: &CertKeyPem,
    domain: &str,
    now: DateTime<Utc>,
) -> Result<CertKeyPem> {
    let ca_key =
        KeyPair::from_pem(&ca.key).map_err(|e| anyhow!("Failed to parse CA key: {}", e))?;
    // Built from the stored certificate, so the leaf names its issuer exactly
    // as the CA names itself
    let issuer = Issuer::from_ca_cert_pem(&ca.cert, ca_key)
        .map_err(|e| anyhow!("Failed to load CA certificate: {}", e))?;

    let wildcard = format!("*.{}", domain);
    let dns = |name: &str| {
        name.to_string()
            .try_into()
            .map(SanType::DnsName)
            .map_err(|e| anyhow!("Invalid domain '{}': {}", name, e))
    };
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|e| anyhow!("Failed to create cert params: {}", e))?;
    params.subject_alt_names = vec![
        dns(domain)?,
        dns(&wildcard)?,
        dns("localhost")?,
        SanType::IpAddress(std::net::IpAddr::from([127, 0, 0, 1])),
    ];
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, domain);
    params
        .distinguished_name
        .push(DnType::OrganizationName, "k3dev");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    set_validity(&mut params, now, LEAF_VALIDITY_DAYS);

    let key = KeyPair::generate().map_err(|e| anyhow!("Failed to generate leaf key: {}", e))?;
    let cert = params
        .signed_by(&key, &issuer)
        .map_err(|e| anyhow!("Failed to sign leaf cert: {}", e))?;
    Ok(CertKeyPem {
        cert: cert.pem(),
        key: key.serialize_pem(),
    })
}

/// Whether a SAN DNS entry covers `domain`, directly or through a wildcard.
fn name_covers(san: &str, domain: &str) -> bool {
    if san == domain {
        return true;
    }
    match san.strip_prefix("*.") {
        Some(base) => base == domain || domain.ends_with(&format!(".{}", base)),
        None => false,
    }
}

/// Check that `leaf_pem` is a current certificate for `domain` issued by `ca_pem`.
/// Returns its expiry, or why it has to be regenerated.
pub(crate) fn check_leaf(
    leaf_pem: &str,
    ca_pem: &str,
    domain: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let leaf_der = pem_to_der(leaf_pem)?;
    let leaf = parse_der(&leaf_der)?;
    let ca_der = pem_to_der(ca_pem)?;
    let ca = parse_der(&ca_der)?;

    let issued_by_ca = leaf.issuer().as_raw() == ca.subject().as_raw()
        && authority_key_id(&leaf).is_some()
        && authority_key_id(&leaf) == subject_key_id(&ca);
    if !issued_by_ca {
        return Err("not issued by the current k3dev CA".to_string());
    }
    match leaf.extended_key_usage() {
        Ok(Some(eku)) if eku.value.server_auth => {}
        _ => return Err("not marked for server authentication".to_string()),
    }
    let span = leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp();
    if span > APPLE_MAX_VALIDITY_DAYS * 86_400 {
        return Err(format!(
            "valid for longer than {} days",
            APPLE_MAX_VALIDITY_DAYS
        ));
    }
    let covered = match leaf.subject_alternative_name() {
        Ok(Some(san)) => san.value.general_names.iter().any(|name| match name {
            GeneralName::DNSName(dns) => name_covers(dns, domain),
            _ => false,
        }),
        _ => false,
    };
    if !covered {
        return Err(format!("does not cover {}", domain));
    }
    check_validity_window(&leaf, now)?;
    Ok(not_after(&leaf))
}

/// SHA-1 hashes of the certificates in `security find-certificate -a -Z -p`
/// output that are not `ca_der` — earlier k3dev CAs to drop from the keychain.
#[cfg(any(target_os = "macos", test))]
fn stale_keychain_hashes(find_output: &str, ca_der: &[u8]) -> Vec<String> {
    find_output
        .split("SHA-256 hash:")
        .filter_map(|record| {
            let sha1 = record
                .lines()
                .find_map(|line| line.trim().strip_prefix("SHA-1 hash:"))?
                .trim()
                .to_string();
            let der = ::pem::parse(record.get(record.find("-----BEGIN")?..)?).ok()?;
            (der.contents() != ca_der && !sha1.is_empty()).then_some(sha1)
        })
        .collect()
}

/// Whether a PEM bundle (such as the system CA bundle) contains `ca_der`.
fn bundle_contains(bundle: &str, ca_der: &[u8]) -> bool {
    ::pem::parse_many(bundle)
        .map(|blocks| blocks.iter().any(|b| b.contents() == ca_der))
        .unwrap_or(false)
}

/// Prefix `argv` with sudo unless already running as root.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn privileged(argv: Vec<String>, is_root: bool) -> Vec<String> {
    if is_root {
        return argv;
    }
    let mut cmd = vec![
        "sudo".to_string(),
        "-p".to_string(),
        "Password to trust the k3dev CA: ".to_string(),
    ];
    cmd.extend(argv);
    cmd
}

/// Removes the keychain certificates given as `$2…` and trusts the CA file in
/// `$1`. Paths travel as arguments, never as script text.
#[cfg(any(target_os = "macos", test))]
const MACOS_TRUST_SCRIPT: &str = r#"ca="$1"; shift
for sha1 in "$@"; do
  security delete-certificate -Z "$sha1" /Library/Keychains/System.keychain >/dev/null 2>&1 || true
done
security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$ca""#;

/// Command line that trusts `ca_path` in the macOS System keychain, removing
/// the `stale` certificates first. Runs under sudo unless already root.
#[cfg(any(target_os = "macos", test))]
fn macos_trust_command(ca_path: &Path, stale: &[String], is_root: bool) -> Vec<String> {
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        MACOS_TRUST_SCRIPT.to_string(),
        "k3dev-trust-ca".to_string(),
        ca_path.to_string_lossy().into_owned(),
    ];
    argv.extend(stale.iter().cloned());
    privileged(argv, is_root)
}

/// Where a Linux distribution takes extra CA anchors, and the command that
/// rebuilds its bundle from them.
#[cfg(any(target_os = "linux", test))]
struct LinuxStore {
    anchor: &'static str,
    update: &'static str,
}

/// Anchor locations by distribution family, checked in order.
#[cfg(target_os = "linux")]
const LINUX_STORES: &[(&str, LinuxStore)] = &[
    (
        "/etc/ca-certificates/trust-source/anchors",
        LinuxStore {
            anchor: "/etc/ca-certificates/trust-source/anchors/k3dev-local-ca.crt",
            update: "update-ca-trust",
        },
    ),
    (
        "/usr/local/share/ca-certificates",
        LinuxStore {
            anchor: "/usr/local/share/ca-certificates/k3dev-local-ca.crt",
            update: "update-ca-certificates",
        },
    ),
    (
        "/etc/pki/ca-trust/source/anchors",
        LinuxStore {
            anchor: "/etc/pki/ca-trust/source/anchors/k3dev-local-ca.pem",
            update: "update-ca-trust",
        },
    ),
];

/// System CA bundles the anchors are compiled into, across distributions.
#[cfg(target_os = "linux")]
const LINUX_BUNDLES: &[&str] = &[
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ca-certificates/extracted/tls-ca-bundle.pem",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/etc/ssl/ca-bundle.pem",
];

/// Copies the CA file in `$1` to the anchor path in `$2`, then runs the
/// bundle update command in `$3`.
#[cfg(any(target_os = "linux", test))]
const LINUX_TRUST_SCRIPT: &str = r#"cp "$1" "$2" && "$3""#;

/// Command line that installs `ca_path` as a system anchor in `store`.
#[cfg(any(target_os = "linux", test))]
fn linux_trust_command(ca_path: &Path, store: &LinuxStore, is_root: bool) -> Vec<String> {
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        LINUX_TRUST_SCRIPT.to_string(),
        "k3dev-trust-ca".to_string(),
        ca_path.to_string_lossy().into_owned(),
        store.anchor.to_string(),
        store.update.to_string(),
    ];
    privileged(argv, is_root)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Longest the trust installation may wait on a password or a system dialog.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Run `argv` so that it can prompt the user: under the TUI its `/dev/tty`
/// is a pty shown in the output popup, headless it keeps the real terminal.
/// Output is streamed to `output_tx`. Returns whether it exited successfully.
async fn run_interactive(argv: &[String], output_tx: &mpsc::Sender<OutputLine>) -> bool {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let pty = match crate::tty::attach_pty(&mut cmd) {
        Ok(pty) => pty,
        Err(e) => {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Cannot open a terminal: {}",
                    e
                )))
                .await;
            return false;
        }
    };
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = output_tx
                .send(OutputLine::warning(format!(
                    "Failed to run {}: {}",
                    program, e
                )))
                .await;
            return false;
        }
    };
    let pty_io = match pty {
        Some(pty) => Some(pty.start_io().await),
        None => None,
    };

    let forward = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>, warn: bool| {
        let tx = output_tx.clone();
        tokio::spawn(async move {
            let Some(stream) = stream else { return };
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = format!("  {}", line);
                let out = if warn {
                    OutputLine::warning(line)
                } else {
                    OutputLine::info(line)
                };
                let _ = tx.send(out).await;
            }
        })
    };
    let stdout = forward(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        false,
    );
    let stderr = forward(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        true,
    );

    let status = tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await;
    if status.is_err() {
        let _ = child.kill().await;
    }
    let _ = stdout.await;
    let _ = stderr.await;
    if let Some(pty_io) = pty_io {
        pty_io.finish().await;
    }
    match status {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        Err(_) => {
            let _ = output_tx
                .send(OutputLine::warning(
                    "Timed out waiting for the CA installation",
                ))
                .await;
            false
        }
    }
}

// --- On-disk CA ---

/// Directory holding the root CA shared by all clusters.
fn ca_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".k3dev")
        .join("ca")
}

/// Path of the root CA certificate.
pub(crate) fn ca_cert_path() -> PathBuf {
    ca_dir().join("rootCA.pem")
}

fn ca_key_path() -> PathBuf {
    ca_dir().join("rootCA-key.pem")
}

/// Hold an exclusive lock on the CA directory, so clusters starting side by
/// side neither regenerate the CA over each other nor prompt twice for sudo.
async fn lock_ca_dir() -> Result<std::fs::File> {
    let dir = ca_dir();
    tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(".lock"))?;
        file.lock()?;
        Ok(file)
    })
    .await
    .map_err(|e| anyhow!("CA lock task failed: {}", e))?
}

/// Write `contents` readable by the owner only — a private key any local user
/// could read would let them mint certificates the system trusts.
pub(crate) async fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, contents.as_bytes()).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `mode` above applies only when the file is created
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    Ok(())
}

/// Load the root CA, generating a new one when it is missing or unusable
/// (expiring, mismatched key, or shaped by an earlier release).
pub(crate) async fn ensure_ca(output_tx: &mpsc::Sender<OutputLine>) -> Result<CertKeyPem> {
    let _lock = lock_ca_dir().await?;
    let (cert_path, key_path) = (ca_cert_path(), ca_key_path());

    if let (Ok(cert), Ok(key)) = (
        tokio::fs::read_to_string(&cert_path).await,
        tokio::fs::read_to_string(&key_path).await,
    ) {
        let ca = CertKeyPem { cert, key };
        match check_ca(&ca, Utc::now()) {
            Ok(()) => return Ok(ca),
            Err(reason) => {
                let _ = output_tx
                    .send(OutputLine::info(format!(
                        "Regenerating k3dev root CA ({})",
                        reason
                    )))
                    .await;
            }
        }
    } else {
        let _ = output_tx
            .send(OutputLine::info("Generating k3dev root CA..."))
            .await;
    }

    let ca = generate_ca(Utc::now())?;
    write_private(&key_path, &ca.key).await?;
    tokio::fs::write(&cert_path, &ca.cert).await?;
    Ok(ca)
}

// --- System trust ---

/// Whether the host trusts the k3dev root CA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustStatus {
    Trusted,
    NotTrusted,
    /// No known trust store to check on this platform
    Unknown,
}

/// Check whether the system trust store accepts the CA certificate at `ca_path`.
pub(crate) async fn trust_status(ca_path: &Path) -> TrustStatus {
    #[cfg(target_os = "macos")]
    {
        // Evaluates the CA against the keychains' trust settings; local only,
        // no network fetches.
        match Command::new("security")
            .args(["verify-cert", "-L", "-q", "-c"])
            .arg(ca_path)
            .output()
            .await
        {
            Ok(out) if out.status.success() => TrustStatus::Trusted,
            Ok(_) => TrustStatus::NotTrusted,
            Err(_) => TrustStatus::Unknown,
        }
    }
    #[cfg(target_os = "linux")]
    {
        let ca_der = match tokio::fs::read_to_string(ca_path).await {
            Ok(pem) => match pem_to_der(&pem) {
                Ok(der) => der,
                Err(_) => return TrustStatus::NotTrusted,
            },
            Err(_) => return TrustStatus::NotTrusted,
        };
        let mut found_bundle = false;
        for bundle in LINUX_BUNDLES {
            if let Ok(text) = tokio::fs::read_to_string(bundle).await {
                found_bundle = true;
                if bundle_contains(&text, &ca_der) {
                    return TrustStatus::Trusted;
                }
            }
        }
        if found_bundle {
            TrustStatus::NotTrusted
        } else {
            TrustStatus::Unknown
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = ca_path;
        TrustStatus::Unknown
    }
}

/// The privileged command that trusts the CA certificate at `ca_path` (whose
/// DER is `ca_der`), if this platform has one.
async fn trust_command(ca_path: &Path, ca_der: &[u8]) -> Option<Vec<String>> {
    #[cfg(target_os = "macos")]
    {
        // Earlier k3dev CAs left in the System keychain go in the same step
        let stale = match Command::new("security")
            .args(["find-certificate", "-a", "-Z", "-p", "-c", CA_COMMON_NAME])
            .arg("/Library/Keychains/System.keychain")
            .output()
            .await
        {
            Ok(out) => stale_keychain_hashes(&String::from_utf8_lossy(&out.stdout), ca_der),
            Err(_) => Vec::new(),
        };
        Some(macos_trust_command(ca_path, &stale, is_root()))
    }
    #[cfg(target_os = "linux")]
    {
        let _ = ca_der;
        let (_, store) = LINUX_STORES
            .iter()
            .find(|(dir, _)| Path::new(dir).is_dir())?;
        Some(linux_trust_command(ca_path, store, is_root()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (ca_path, ca_der);
        None
    }
}

/// Make sure the system trusts the k3dev root CA, asking for sudo to install
/// it when it does not. Never fails the start: an untrusted CA only means
/// browsers warn, and the next start asks again.
pub(crate) async fn ensure_trusted(
    ca: &CertKeyPem,
    output_tx: &mpsc::Sender<OutputLine>,
) -> TrustStatus {
    // Serialized with other clusters, so a concurrent start sees the result
    // instead of prompting a second time
    let _lock = lock_ca_dir().await.ok();
    let ca_path = ca_cert_path();

    let mut status = trust_status(&ca_path).await;
    if status == TrustStatus::NotTrusted {
        let ca_der = pem_to_der(&ca.cert).unwrap_or_default();
        match trust_command(&ca_path, &ca_der).await {
            Some(argv) => {
                let _ = output_tx
                    .send(OutputLine::info(
                        "The k3dev root CA is not trusted by this system yet — installing it (requires sudo)...",
                    ))
                    .await;
                #[cfg(target_os = "macos")]
                let _ = output_tx
                    .send(OutputLine::info(
                        "macOS may also ask you to confirm the trust change in a system dialog.",
                    ))
                    .await;
                run_interactive(&argv, output_tx).await;
                status = trust_status(&ca_path).await;
            }
            None => {
                let _ = output_tx
                    .send(OutputLine::warning(
                        "No known system trust store to install the k3dev root CA into",
                    ))
                    .await;
            }
        }
        match status {
            TrustStatus::Trusted => {
                let _ = output_tx
                    .send(OutputLine::success(
                        "k3dev root CA installed into the system trust store",
                    ))
                    .await;
            }
            _ => {
                let _ = output_tx
                    .send(OutputLine::warning(format!(
                        "k3dev root CA is not trusted, so HTTPS ingress shows as not secure. It will be offered again on the next start, or trust it manually: {}",
                        manual_trust_hint(&ca_path)
                    )))
                    .await;
            }
        }
    } else if status == TrustStatus::Unknown {
        let _ = output_tx
            .send(OutputLine::warning(format!(
                "Cannot check whether this system trusts the k3dev root CA; if browsers reject the certificate, trust {} manually",
                ca_path.display()
            )))
            .await;
    }

    // Browsers with their own NSS databases (Firefox, Chromium on Linux)
    sync_nss_databases(ca, output_tx).await;
    status
}

/// A command a user can run to trust the CA by hand.
pub(crate) fn manual_trust_hint(ca_path: &Path) -> String {
    if cfg!(target_os = "macos") {
        format!(
            "sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain '{}'",
            ca_path.display()
        )
    } else if cfg!(target_os = "linux") {
        format!(
            "sudo cp '{}' /usr/local/share/ca-certificates/k3dev-local-ca.crt && sudo update-ca-certificates",
            ca_path.display()
        )
    } else {
        format!(
            "add {} to the system's trusted root certificates",
            ca_path.display()
        )
    }
}

/// NSS nickname the CA is stored under.
const NSS_NICKNAME: &str = CA_COMMON_NAME;

/// NSS databases of the user's browsers that keep their own trust store.
fn nss_databases() -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut dbs = Vec::new();

    // Chrome/Chromium on Linux
    let chrome = home.join(".pki").join("nssdb");
    if chrome.exists() {
        dbs.push(format!("sql:{}", chrome.display()));
    }

    #[cfg(target_os = "macos")]
    let firefox = home
        .join("Library")
        .join("Application Support")
        .join("Firefox")
        .join("Profiles");
    #[cfg(not(target_os = "macos"))]
    let firefox = home.join(".mozilla").join("firefox");
    if let Ok(entries) = std::fs::read_dir(&firefox) {
        for path in entries.flatten().map(|e| e.path()) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if path.is_dir() && name.contains("default") {
                dbs.push(format!("sql:{}", path.display()));
            }
        }
    }
    dbs
}

/// Keep the CA current in browser NSS databases, replacing an earlier k3dev
/// CA stored under the same nickname. No sudo needed: they are per user.
async fn sync_nss_databases(ca: &CertKeyPem, output_tx: &mpsc::Sender<OutputLine>) {
    if super::platform::PlatformInfo::find_binary("certutil").is_none() {
        return;
    }
    let Ok(ca_der) = pem_to_der(&ca.cert) else {
        return;
    };
    let ca_path = ca_cert_path();

    for db in nss_databases() {
        let current = Command::new("certutil")
            .args(["-d", &db, "-L", "-a", "-n", NSS_NICKNAME])
            .output()
            .await;
        if let Ok(out) = &current {
            if out.status.success()
                && bundle_contains(&String::from_utf8_lossy(&out.stdout), &ca_der)
            {
                continue;
            }
        }
        // Drop every earlier copy; NSS keeps several under one nickname
        for _ in 0..8 {
            let deleted = Command::new("certutil")
                .args(["-d", &db, "-D", "-n", NSS_NICKNAME])
                .output()
                .await;
            if !matches!(deleted, Ok(out) if out.status.success()) {
                break;
            }
        }
        let added = Command::new("certutil")
            .args(["-d", &db, "-A", "-t", "C,,", "-n", NSS_NICKNAME, "-i"])
            .arg(&ca_path)
            .output()
            .await;
        if matches!(added, Ok(out) if out.status.success()) {
            let _ = output_tx
                .send(OutputLine::info(format!("Trusted k3dev root CA in {}", db)))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    };

    const DOMAIN: &str = "local.k8s.dev";

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn der(pem_str: &str) -> Vec<u8> {
        ::pem::parse(pem_str).unwrap().into_contents()
    }

    fn with_cert<R>(pem_str: &str, f: impl FnOnce(&X509Certificate<'_>) -> R) -> R {
        let der = der(pem_str);
        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        f(&cert)
    }

    fn ski(cert: &X509Certificate<'_>) -> Option<Vec<u8>> {
        cert.extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::SubjectKeyIdentifier(k) => Some(k.0.to_vec()),
                _ => None,
            })
    }

    fn aki(cert: &X509Certificate<'_>) -> Option<Vec<u8>> {
        cert.extensions()
            .iter()
            .find_map(|e| match e.parsed_extension() {
                ParsedExtension::AuthorityKeyIdentifier(a) => {
                    a.key_identifier.as_ref().map(|k| k.0.to_vec())
                }
                _ => None,
            })
    }

    fn validity_days(cert: &X509Certificate<'_>) -> i64 {
        (cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp()) / 86_400
    }

    /// A CA shaped like the ones earlier k3dev releases generated: rcgen
    /// defaults, so no key usage and a 1975–4096 validity.
    fn legacy_ca() -> CertKeyPem {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, CA_COMMON_NAME);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "k3dev");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        CertKeyPem {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    /// A leaf shaped like earlier releases generated: SANs only.
    fn legacy_leaf(ca: &CertKeyPem) -> String {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, CA_COMMON_NAME);
        ca_params
            .distinguished_name
            .push(DnType::OrganizationName, "k3dev");
        let issuer = Issuer::new(ca_params, KeyPair::from_pem(&ca.key).unwrap());
        let params =
            CertificateParams::new(vec![DOMAIN.to_string(), format!("*.{}", DOMAIN)]).unwrap();
        let key = KeyPair::generate().unwrap();
        params.signed_by(&key, &issuer).unwrap().pem()
    }

    // --- CA ---

    #[test]
    fn generated_ca_allows_certificate_signing() {
        let ca = generate_ca(now()).unwrap();
        with_cert(&ca.cert, |cert| {
            assert!(cert.is_ca());
            let ku = cert
                .key_usage()
                .unwrap()
                .expect("CA must carry a key usage");
            assert!(ku.value.key_cert_sign());
            assert!(
                ski(cert).is_some(),
                "CA must carry a subject key identifier"
            );
        });
    }

    #[test]
    fn generated_ca_starts_in_the_past_and_has_bounded_validity() {
        let ca = generate_ca(now()).unwrap();
        with_cert(&ca.cert, |cert| {
            assert!(cert.validity().not_before.timestamp() <= now().timestamp());
            let days = validity_days(cert);
            assert!((3000..=4000).contains(&days), "CA validity {} days", days);
        });
    }

    #[test]
    fn check_ca_accepts_freshly_generated_ca() {
        let ca = generate_ca(now()).unwrap();
        assert_eq!(check_ca(&ca, now()), Ok(()));
    }

    #[test]
    fn check_ca_rejects_ca_without_key_usage() {
        let err = check_ca(&legacy_ca(), now()).unwrap_err();
        assert!(err.contains("key usage"), "{}", err);
    }

    #[test]
    fn check_ca_rejects_key_of_another_ca() {
        let ca = generate_ca(now()).unwrap();
        let other = generate_ca(now()).unwrap();
        let mixed = CertKeyPem {
            cert: ca.cert,
            key: other.key,
        };
        let err = check_ca(&mixed, now()).unwrap_err();
        assert!(err.contains("key"), "{}", err);
    }

    #[test]
    fn check_ca_rejects_ca_close_to_expiry() {
        let ca = generate_ca(now()).unwrap();
        let near_end = now() + chrono::Duration::days(3650 - 5);
        assert!(check_ca(&ca, near_end).is_err());
    }

    // --- Leaf ---

    #[test]
    fn check_leaf_accepts_freshly_generated_leaf() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        assert!(check_leaf(&leaf.cert, &ca.cert, DOMAIN, now()).is_ok());
    }

    #[test]
    fn generated_leaf_is_for_server_auth_and_names_its_issuer_key() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        let ca_ski = with_cert(&ca.cert, ski).unwrap();
        with_cert(&leaf.cert, |cert| {
            let eku = cert
                .extended_key_usage()
                .unwrap()
                .expect("leaf must carry an EKU");
            assert!(eku.value.server_auth);
            let ku = cert
                .key_usage()
                .unwrap()
                .expect("leaf must carry a key usage");
            assert!(ku.value.digital_signature());
            assert!(!cert.is_ca());
            assert_eq!(aki(cert), Some(ca_ski));
        });
    }

    #[test]
    fn generated_leaf_validity_fits_apple_limit() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        with_cert(&leaf.cert, |cert| {
            assert!(cert.validity().not_before.timestamp() <= now().timestamp());
            assert!(validity_days(cert) <= 825, "{} days", validity_days(cert));
        });
    }

    #[test]
    fn generated_leaf_covers_domain_wildcard_and_localhost() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        with_cert(&leaf.cert, |cert| {
            let san = cert.subject_alternative_name().unwrap().unwrap();
            let names: Vec<String> = san
                .value
                .general_names
                .iter()
                .map(|n| match n {
                    GeneralName::DNSName(d) => d.to_string(),
                    GeneralName::IPAddress(ip) => format!("{:?}", ip),
                    other => format!("{:?}", other),
                })
                .collect();
            assert!(names.contains(&DOMAIN.to_string()));
            assert!(names.contains(&format!("*.{}", DOMAIN)));
            assert!(names.contains(&"localhost".to_string()));
            assert!(names.contains(&format!("{:?}", &[127u8, 0, 0, 1][..])));
        });
    }

    #[test]
    fn check_leaf_rejects_leaf_of_another_ca() {
        let ca = generate_ca(now()).unwrap();
        let other = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&other, DOMAIN, now()).unwrap();
        assert!(check_leaf(&leaf.cert, &ca.cert, DOMAIN, now()).is_err());
    }

    #[test]
    fn check_leaf_rejects_leaf_from_earlier_release() {
        let ca = legacy_ca();
        let leaf = legacy_leaf(&ca);
        assert!(check_leaf(&leaf, &ca.cert, DOMAIN, now()).is_err());
    }

    #[test]
    fn check_leaf_rejects_other_domain() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, "other.dev", now()).unwrap();
        let err = check_leaf(&leaf.cert, &ca.cert, DOMAIN, now()).unwrap_err();
        assert!(err.contains(DOMAIN), "{}", err);
    }

    #[test]
    fn check_leaf_accepts_subdomain_covered_by_wildcard() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        assert!(check_leaf(&leaf.cert, &ca.cert, "app.local.k8s.dev", now()).is_ok());
    }

    #[test]
    fn check_leaf_rejects_leaf_close_to_expiry() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        let near_end = now() + chrono::Duration::days(820 - 5);
        assert!(check_leaf(&leaf.cert, &ca.cert, DOMAIN, near_end).is_err());
    }

    /// Run `openssl verify -x509_strict` on a chain — the checks Python 3.13+
    /// applies by default. `None` where no OpenSSL 3 is installed (LibreSSL's
    /// verify lacks most strict checks, so it would prove nothing).
    fn openssl_strict_verify(ca_pem: &str, leaf_pem: &str) -> Option<Result<(), String>> {
        let version = std::process::Command::new("openssl")
            .arg("version")
            .output()
            .ok()?;
        if !String::from_utf8_lossy(&version.stdout).starts_with("OpenSSL 3") {
            return None;
        }
        let dir = std::env::temp_dir().join(format!(
            "k3dev-ca-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ca_path = dir.join("ca.pem");
        let leaf_path = dir.join("leaf.pem");
        std::fs::write(&ca_path, ca_pem).unwrap();
        std::fs::write(&leaf_path, leaf_pem).unwrap();
        let out = std::process::Command::new("openssl")
            .args(["verify", "-x509_strict", "-purpose", "sslserver"])
            .arg("-verify_hostname")
            .arg(format!("app.{}", DOMAIN))
            .arg("-CAfile")
            .arg(&ca_path)
            .arg(&leaf_path)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        Some(if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ))
        })
    }

    /// Distinct per call, so tests running in parallel never share a directory.
    fn unique_suffix() -> usize {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn generated_chain_passes_openssl_strict_verification() {
        let ca = generate_ca(now()).unwrap();
        let leaf = generate_leaf(&ca, DOMAIN, now()).unwrap();
        if let Some(result) = openssl_strict_verify(&ca.cert, &leaf.cert) {
            assert_eq!(result, Ok(()));
        }
    }

    /// Guards the check above: the chain earlier releases generated fails it.
    #[test]
    fn legacy_chain_fails_openssl_strict_verification() {
        let ca = legacy_ca();
        let leaf = legacy_leaf(&ca);
        if let Some(result) = openssl_strict_verify(&ca.cert, &leaf) {
            assert!(result.is_err());
        }
    }

    // --- Trust stores ---

    fn keychain_record(sha1: &str, cert_pem: &str) -> String {
        format!("SHA-256 hash: {sha1}{sha1}\nSHA-1 hash: {sha1}\n{cert_pem}")
    }

    #[test]
    fn stale_keychain_hashes_lists_only_other_cas() {
        let current = generate_ca(now()).unwrap();
        let old = legacy_ca();
        let output = format!(
            "{}{}",
            keychain_record("AAAA", &current.cert),
            keychain_record("BBBB", &old.cert)
        );
        assert_eq!(
            stale_keychain_hashes(&output, &der(&current.cert)),
            vec!["BBBB".to_string()]
        );
    }

    #[test]
    fn stale_keychain_hashes_is_empty_without_matches() {
        let current = generate_ca(now()).unwrap();
        assert!(stale_keychain_hashes("", &der(&current.cert)).is_empty());
    }

    #[test]
    fn bundle_contains_finds_ca_among_others() {
        let ca = generate_ca(now()).unwrap();
        let other = generate_ca(now()).unwrap();
        let bundle = format!("# Some root\n{}\n# k3dev\n{}", other.cert, ca.cert);
        assert!(bundle_contains(&bundle, &der(&ca.cert)));
        assert!(!bundle_contains(&other.cert, &der(&ca.cert)));
    }

    #[test]
    fn macos_trust_command_passes_paths_as_arguments() {
        let path = Path::new("/Users/a b/it's/.k3dev/ca/rootCA.pem");
        let argv = macos_trust_command(path, &["BBBB".to_string()], false);
        assert_eq!(argv[0], "sudo");
        let sh = argv.iter().position(|a| a == "sh").unwrap();
        assert_eq!(argv[sh + 1], "-c");
        let script = &argv[sh + 2];
        assert!(script.contains("add-trusted-cert"));
        assert!(script.contains("/Library/Keychains/System.keychain"));
        // The path never becomes part of the script text
        assert!(!script.contains("rootCA.pem"));
        assert_eq!(argv[sh + 4], path.to_string_lossy());
        assert_eq!(argv[sh + 5], "BBBB");
    }

    #[test]
    fn macos_trust_command_skips_sudo_for_root() {
        let argv = macos_trust_command(Path::new("/root/ca.pem"), &[], true);
        assert_eq!(argv[0], "sh");
    }

    #[test]
    fn linux_trust_command_passes_paths_as_arguments() {
        let store = LinuxStore {
            anchor: "/usr/local/share/ca-certificates/k3dev-local-ca.crt",
            update: "update-ca-certificates",
        };
        let path = Path::new("/home/a b/it's/.k3dev/ca/rootCA.pem");
        let argv = linux_trust_command(path, &store, false);
        assert_eq!(argv[0], "sudo");
        let sh = argv.iter().position(|a| a == "sh").unwrap();
        let script = &argv[sh + 2];
        assert!(!script.contains("rootCA.pem"));
        assert!(!script.contains("update-ca-certificates"));
        assert_eq!(
            &argv[sh + 4..],
            &[
                path.to_string_lossy().into_owned(),
                store.anchor.to_string(),
                store.update.to_string()
            ]
        );
    }

    #[tokio::test]
    async fn run_interactive_streams_output_and_reports_failure() {
        let _guard = crate::tty::test_guard().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let argv: Vec<String> = ["sh", "-c", "echo to-stdout; echo to-stderr >&2; exit 3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let ok = run_interactive(&argv, &tx).await;
        drop(tx);
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line.content);
        }
        assert!(!ok);
        assert!(lines.iter().any(|l| l.contains("to-stdout")), "{:?}", lines);
        assert!(lines.iter().any(|l| l.contains("to-stderr")), "{:?}", lines);
    }

    #[tokio::test]
    async fn run_interactive_reports_success() {
        let _guard = crate::tty::test_guard().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let argv = vec!["true".to_string()];
        assert!(run_interactive(&argv, &tx).await);
    }

    #[tokio::test]
    async fn run_interactive_reports_missing_program_as_failure() {
        let _guard = crate::tty::test_guard().await;
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let argv = vec!["k3dev-no-such-program".to_string()];
        assert!(!run_interactive(&argv, &tx).await);
    }
}
