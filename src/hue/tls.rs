use anyhow::{anyhow, bail, Context, Result};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode};
use openssl::stack::{Stack, StackRef};
use openssl::x509::store::{X509Store, X509StoreBuilder, X509StoreRef};
use openssl::x509::{X509Ref, X509StoreContext, X509};
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant};

const HTTPS_PORT: u16 = 443;

/// What stands behind the bridge's HTTPS certificate. Saved as `hue_bridge_kind`; a config
/// without the field is a real bridge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeKind {
    /// A Philips Hue bridge: its certificate chains to the official Hue roots.
    #[default]
    Real,
    /// A bridge emulator (for example Bifrost) with a self-signed certificate. Only an explicit
    /// pairing choice selects it. Its only trust is an exact match of the leaf certificate
    /// SHA-256 pinned at pairing: no CA path and no fallback.
    Emulated,
}

impl BridgeKind {
    pub fn is_real(&self) -> bool {
        *self == Self::Real
    }
}

/// A saved identity accepts CA-signed certificate renewal; a legacy pin bootstraps that identity.
/// An emulated bridge needs its pairing pin on every connection.
#[derive(Clone, Copy, Default)]
pub struct BridgeTrust<'a> {
    pub bridge_id: Option<&'a str>,
    pub certificate_sha256: Option<&'a str>,
    pub kind: BridgeKind,
}

impl BridgeTrust<'_> {
    pub fn known(self) -> bool {
        match self.kind {
            BridgeKind::Real => self.bridge_id.is_some() || self.certificate_sha256.is_some(),
            BridgeKind::Emulated => self.certificate_sha256.is_some(),
        }
    }
}

pub fn connect_bridge_tls(bridge_ip: &str, trust: BridgeTrust<'_>) -> Result<SslStream<TcpStream>> {
    connect_bridge_tls_on(bridge_ip, HTTPS_PORT, trust)
}

/// The certificate an emulated bridge presented at pairing.
#[derive(Debug)]
pub struct EmulatedIdentity {
    pub certificate_sha256: String,
    /// The certificate chains to the official Hue roots: a real Philips bridge stands behind
    /// the address, and real-bridge mode is the right choice for it.
    pub signed_by_hue_roots: bool,
}

/// Emulated pairing sends the stream to the address the user typed, with no CA behind it, so
/// only a LAN address is accepted: private (RFC 1918) or link-local IPv4. Every pairing entry
/// point (CLI, dashboard, `pair_hue.py`) reaches this check through `pair_bridge`.
pub fn emulated_bridge_address(bridge_ip: &str) -> Result<IpAddr> {
    match bridge_ip.trim().parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) if ip.is_private() || ip.is_link_local() => Ok(IpAddr::V4(ip)),
        Ok(_) => bail!(
            "Emulated Hue Bridge IP must be a private IPv4 address (10.x, 172.16-31.x, 192.168.x or 169.254.x)"
        ),
        Err(_) => bail!("Hue Bridge address must be an IP literal"),
    }
}

/// Trust on first use, for an explicit emulated-bridge pairing only: reads the certificate of
/// a bridge that cannot chain to the Hue roots and returns its leaf SHA-256. The bridge ID
/// comes from the first pinned request, which also checks it.
/// The caller shows the pin to the user, and every later request must match it exactly.
pub fn emulated_bridge_identity(bridge_ip: &str) -> Result<EmulatedIdentity> {
    let ip = emulated_bridge_address(bridge_ip)?;
    emulated_bridge_identity_on(&ip.to_string(), HTTPS_PORT)
}

fn emulated_bridge_identity_on(bridge_ip: &str, port: u16) -> Result<EmulatedIdentity> {
    let ip: IpAddr = bridge_ip
        .parse()
        .context("Hue Bridge address must be an IP literal")?;
    // No rediscovery: the user named this address when choosing to trust it.
    let tcp = TcpStream::connect_timeout(&(ip, port).into(), Duration::from_secs(5))
        .context("Failed to connect to the emulated Hue Bridge HTTPS endpoint")?;
    let stream = handshake(bridge_ip, tcp, BridgeKind::Emulated)?;
    Ok(EmulatedIdentity {
        certificate_sha256: peer_certificate_sha256(&stream)?,
        signed_by_hue_roots: peer_signed_by_hue_roots(&stream)?,
    })
}

/// Checks the peer certificate, with the chain the peer sent, against the embedded Hue roots.
/// Only informs the user: the emulated handshake never uses this as trust.
fn peer_signed_by_hue_roots(stream: &SslStream<TcpStream>) -> Result<bool> {
    let leaf = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("Hue Bridge did not present a TLS certificate"))?;
    let mut chain = Stack::new()?;
    for certificate in stream.ssl().peer_cert_chain().into_iter().flatten() {
        chain.push(certificate.to_owned())?;
    }
    let roots = bridge_roots()?;
    chains_to(&roots, &leaf, &chain)
}

fn chains_to(roots: &X509StoreRef, leaf: &X509Ref, chain: &StackRef<X509>) -> Result<bool> {
    let mut context = X509StoreContext::new()?;
    Ok(context.init(roots, leaf, chain, |context| context.verify_cert())?)
}

fn connect_bridge_tls_on(
    bridge_ip: &str,
    port: u16,
    trust: BridgeTrust<'_>,
) -> Result<SslStream<TcpStream>> {
    if trust.kind == BridgeKind::Emulated && !trust.certificate_sha256.is_some_and(valid_sha256) {
        bail!("Emulated Hue Bridge has no certificate pin; pair it again as an emulated bridge");
    }
    let ip: IpAddr = bridge_ip
        .parse()
        .context("Hue Bridge address must be an IP literal")?;
    let tcp = match TcpStream::connect_timeout(&(ip, port).into(), Duration::from_secs(5)) {
        Ok(tcp) => tcp,
        Err(error) => {
            let Some(id) = trust.bridge_id else {
                return Err(error).context("Failed to connect to Hue Bridge HTTPS endpoint");
            };
            // Rediscovery is only for an unreachable address, never a TLS or authorization failure.
            // An emulated bridge at the rediscovered address must still match its pin below.
            let discovered = super::discovery::discover_bridge_for(Some(id))
                .map_err(|cause| unreachable_bridge(bridge_ip, error, cause))?;
            let address: IpAddr = discovered
                .parse()
                .context("Invalid discovered Hue address")?;
            TcpStream::connect_timeout(&(address, port).into(), Duration::from_secs(5))
                .context("Failed to connect to the rediscovered Hue Bridge")?
        }
    };
    let stream = handshake(bridge_ip, tcp, trust.kind)?;
    let id = peer_bridge_id(&stream)?;
    let fingerprint = peer_certificate_sha256(&stream)?;
    match trust.kind {
        BridgeKind::Real => check_identity(&id, &fingerprint, trust)?,
        BridgeKind::Emulated => check_pinned_identity(&id, &fingerprint, trust)?,
    }
    Ok(stream)
}

fn handshake(bridge_ip: &str, tcp: TcpStream, kind: BridgeKind) -> Result<SslStream<TcpStream>> {
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let _deadline = super::SocketDeadline::new(&tcp, Instant::now() + Duration::from_secs(10))?;
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    match kind {
        BridgeKind::Real => {
            builder.set_cert_store(bridge_roots()?);
            builder.set_verify(SslVerifyMode::PEER);
        }
        // No CA stands behind an emulator. The caller checks the exact leaf pin before any
        // request is written; nothing is sent on this connection before that check.
        BridgeKind::Emulated => builder.set_verify(SslVerifyMode::NONE),
    }
    let mut configuration = builder.build().configure()?;
    // Hue certificates identify the bridge by CN, not its DHCP address; checked by the caller.
    configuration.set_verify_hostname(false);
    let failure = match kind {
        BridgeKind::Real => {
            "Hue Bridge TLS certificate validation failed; update bridge firmware or check its identity"
        }
        BridgeKind::Emulated => "Emulated Hue Bridge TLS handshake failed",
    };
    configuration.connect(bridge_ip, tcp).context(failure)
}

/// Keeps the TCP failure as the root cause: rediscovery is only the fallback that also failed.
fn unreachable_bridge(
    bridge_ip: &str,
    connect: std::io::Error,
    rediscovery: anyhow::Error,
) -> anyhow::Error {
    anyhow::Error::new(connect).context(format!(
        "Hue Bridge rediscovery failed ({rediscovery:#}) after the configured bridge IP {bridge_ip} was unreachable on port 443"
    ))
}

fn bridge_roots() -> Result<X509Store> {
    let mut roots = X509StoreBuilder::new()?;
    for ca in X509::stack_from_pem(include_bytes!("bridge-ca.pem"))? {
        roots.add_cert(ca)?;
    }
    Ok(roots.build())
}

fn check_identity(id: &str, fingerprint: &str, trust: BridgeTrust<'_>) -> Result<()> {
    if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Hue Bridge certificate has an invalid bridge ID");
    }
    if let Some(expected) = trust.bridge_id {
        if !expected.eq_ignore_ascii_case(id) {
            bail!("Hue Bridge identity changed; pair the replacement bridge explicitly");
        }
    } else if let Some(expected) = trust.certificate_sha256 {
        if !expected.eq_ignore_ascii_case(fingerprint) {
            bail!("Hue Bridge certificate fingerprint changed before identity migration; re-pair explicitly");
        }
    }
    Ok(())
}

/// An emulated bridge: the leaf certificate must match the pairing pin exactly, and the saved
/// bridge ID when one exists. No CA signature and no other certificate is ever accepted.
fn check_pinned_identity(id: &str, fingerprint: &str, trust: BridgeTrust<'_>) -> Result<()> {
    let Some(pin) = trust.certificate_sha256.filter(|pin| valid_sha256(pin)) else {
        bail!("Emulated Hue Bridge has no certificate pin; pair it again as an emulated bridge");
    };
    if !pin.eq_ignore_ascii_case(fingerprint) {
        bail!("Emulated Hue Bridge certificate does not match the pin saved at pairing; re-pair explicitly");
    }
    if let Some(expected) = trust.bridge_id {
        if !expected.eq_ignore_ascii_case(id) {
            bail!("Hue Bridge identity changed; pair the replacement bridge explicitly");
        }
    }
    Ok(())
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn peer_bridge_id(stream: &SslStream<TcpStream>) -> Result<String> {
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("Hue Bridge did not present a TLS certificate"))?;
    let mut names = certificate.subject_name().entries_by_nid(Nid::COMMONNAME);
    let id = names
        .next()
        .ok_or_else(|| anyhow!("Hue Bridge certificate has no bridge ID"))?
        .data()
        .to_string()?
        .to_ascii_lowercase();
    if names.next().is_some() || id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Hue Bridge certificate has an invalid bridge ID");
    }
    Ok(id)
}

pub fn peer_certificate_sha256(stream: &SslStream<TcpStream>) -> Result<String> {
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("Hue Bridge did not present a TLS certificate"))?;
    Ok(certificate
        .digest(MessageDigest::sha256())?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::pkey::{PKey, Private};
    use openssl::ssl::SslAcceptor;
    use std::io::Read;

    /// A self-signed certificate like an emulator's: a bridge-ID CN and no Hue CA.
    fn self_signed(common_name: &str) -> (X509, PKey<Private>) {
        let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let ec = openssl::ec::EcKey::generate(&group).unwrap();
        let key = PKey::from_ec_key(ec).unwrap();
        let mut name = openssl::x509::X509Name::builder().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, common_name)
            .unwrap();
        let name = name.build();
        let mut certificate = X509::builder().unwrap();
        certificate.set_version(2).unwrap();
        certificate.set_subject_name(&name).unwrap();
        certificate.set_issuer_name(&name).unwrap();
        certificate.set_pubkey(&key).unwrap();
        certificate
            .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        certificate
            .set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        certificate.sign(&key, MessageDigest::sha256()).unwrap();
        (certificate.build(), key)
    }

    fn sha256_hex(certificate: &X509) -> String {
        certificate
            .digest(MessageDigest::sha256())
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Serves one TLS connection with `identity` on a loopback port, until the client closes.
    fn serve_once(identity: &(X509, PKey<Private>)) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_certificate(&identity.0).unwrap();
        acceptor.set_private_key(&identity.1).unwrap();
        let acceptor = acceptor.build();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            if let Ok(mut stream) = acceptor.accept(tcp) {
                let _ = stream.read(&mut [0; 1]);
            }
        });
        (port, server)
    }

    fn connect_once(
        identity: &(X509, PKey<Private>),
        trust: BridgeTrust<'_>,
    ) -> Result<SslStream<TcpStream>> {
        let (port, server) = serve_once(identity);
        let result = connect_bridge_tls_on("127.0.0.1", port, trust);
        if let Ok(stream) = &result {
            let _ = stream.get_ref().shutdown(std::net::Shutdown::Both);
        }
        server.join().unwrap();
        result
    }

    #[test]
    fn emulated_bridge_pairs_on_first_use_then_needs_the_exact_pin() {
        let id = "001788fffe000001";
        let identity = self_signed(id);
        let (port, server) = serve_once(&identity);
        let seen = emulated_bridge_identity_on("127.0.0.1", port).unwrap();
        server.join().unwrap();
        assert_eq!(seen.certificate_sha256, sha256_hex(&identity.0));
        assert!(!seen.signed_by_hue_roots);
        let pin = seen.certificate_sha256;

        let pinned = BridgeTrust {
            bridge_id: Some(id),
            certificate_sha256: Some(&pin),
            kind: BridgeKind::Emulated,
        };
        assert!(pinned.known());
        assert!(connect_once(&identity, pinned).is_ok());
        let upper = pin.to_ascii_uppercase();
        let pin_only = BridgeTrust {
            bridge_id: None,
            certificate_sha256: Some(&upper),
            kind: BridgeKind::Emulated,
        };
        assert!(connect_once(&identity, pin_only).is_ok());
    }

    #[test]
    fn emulated_bridge_refuses_another_certificate_identity_or_a_missing_pin() {
        let id = "001788fffe000001";
        let identity = self_signed(id);
        let pin = sha256_hex(&identity.0);

        // Same bridge ID, new key: the pin, not the CN, is the trust.
        let replaced = self_signed(id);
        let error = connect_once(
            &replaced,
            BridgeTrust {
                bridge_id: Some(id),
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Emulated,
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("does not match the pin"),
            "{error:#}"
        );

        let error = connect_once(
            &identity,
            BridgeTrust {
                bridge_id: Some("001788fffe000002"),
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Emulated,
            },
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("identity changed"),
            "{error:#}"
        );

        // No pin or a malformed pin: refused before any connection, even with a saved bridge
        // ID. The address is not an IP literal, so without the early pin check the call would
        // fail on the address instead, and it can never reach the network or rediscovery.
        let unpinned = BridgeTrust {
            bridge_id: Some(id),
            certificate_sha256: None,
            kind: BridgeKind::Emulated,
        };
        assert!(!unpinned.known());
        for pin in [None, Some("not-a-sha256"), Some(&pin[1..])] {
            let trust = BridgeTrust {
                certificate_sha256: pin,
                ..unpinned
            };
            let error = connect_bridge_tls_on("pin-check-runs-first", 9, trust).unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                "Emulated Hue Bridge has no certificate pin; pair it again as an emulated bridge"
            );
        }
    }

    #[test]
    fn emulated_pairing_accepts_only_private_or_link_local_ipv4() {
        for ip in [
            "10.0.0.5",
            "172.16.1.2",
            "172.31.255.1",
            " 192.168.1.20 ",
            "169.254.3.4",
        ] {
            assert_eq!(emulated_bridge_address(ip).unwrap().to_string(), ip.trim());
        }
        for ip in [
            "8.8.8.8",
            "192.0.2.10",
            "172.32.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "fe80::1",
            "fd00::1",
            "::1",
        ] {
            let error = emulated_bridge_address(ip).unwrap_err();
            assert!(
                format!("{error:#}").contains("must be a private IPv4 address"),
                "{ip}: {error:#}"
            );
        }
        assert!(emulated_bridge_address("bifrost.local").is_err());
        // The public pairing entry point refuses before it opens any connection.
        let error = emulated_bridge_identity("8.8.8.8").unwrap_err();
        assert!(format!("{error:#}").contains("must be a private IPv4 address"));
    }

    #[test]
    fn philips_signed_certificates_are_told_apart_from_self_signed_ones() {
        // A test CA stands in for the Hue roots: no test can hold a Philips-signed key.
        let key = || {
            let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            PKey::from_ec_key(openssl::ec::EcKey::generate(&group).unwrap()).unwrap()
        };
        let ca_key = key();
        let mut ca_name = openssl::x509::X509Name::builder().unwrap();
        ca_name
            .append_entry_by_nid(Nid::COMMONNAME, "test-hue-root")
            .unwrap();
        let ca_name = ca_name.build();
        let mut ca = X509::builder().unwrap();
        ca.set_version(2).unwrap();
        ca.set_subject_name(&ca_name).unwrap();
        ca.set_issuer_name(&ca_name).unwrap();
        ca.set_pubkey(&ca_key).unwrap();
        ca.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        ca.set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        ca.append_extension(
            openssl::x509::extension::BasicConstraints::new()
                .critical()
                .ca()
                .build()
                .unwrap(),
        )
        .unwrap();
        ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
        let ca = ca.build();
        let leaf_key = key();
        let mut name = openssl::x509::X509Name::builder().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "001788fffe000001")
            .unwrap();
        let mut leaf = X509::builder().unwrap();
        leaf.set_version(2).unwrap();
        leaf.set_subject_name(&name.build()).unwrap();
        leaf.set_issuer_name(ca.subject_name()).unwrap();
        leaf.set_pubkey(&leaf_key).unwrap();
        leaf.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        leaf.set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
        let leaf = leaf.build();

        let mut roots = X509StoreBuilder::new().unwrap();
        roots.add_cert(ca).unwrap();
        let roots = roots.build();
        let empty = Stack::new().unwrap();
        assert!(chains_to(&roots, &leaf, &empty).unwrap());
        let (emulator, _) = self_signed("001788fffe000001");
        assert!(!chains_to(&roots, &emulator, &empty).unwrap());
        assert!(!chains_to(&bridge_roots().unwrap(), &leaf, &empty).unwrap());
    }

    #[test]
    fn real_bridge_still_requires_the_hue_roots_even_with_a_matching_pin() {
        let id = "001788fffe000001";
        let identity = self_signed(id);
        let pin = sha256_hex(&identity.0);
        for trust in [
            BridgeTrust::default(),
            BridgeTrust {
                bridge_id: Some(id),
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            },
            BridgeTrust {
                bridge_id: None,
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            },
        ] {
            let error = connect_once(&identity, trust).unwrap_err();
            assert!(
                format!("{error:#}").contains("certificate validation failed"),
                "{error:#}"
            );
        }
    }

    #[test]
    fn trust_requires_matching_identity_or_legacy_pin_and_embeds_both_roots() {
        let roots = X509::stack_from_pem(include_bytes!("bridge-ca.pem")).unwrap();
        assert_eq!(roots.len(), 2);
        for root in roots {
            assert!(root.verify(&root.public_key().unwrap()).unwrap());
        }
        let (untrusted, _) = self_signed("001788fffe000001");
        let mut context = openssl::x509::X509StoreContext::new().unwrap();
        assert!(!context
            .init(
                &bridge_roots().unwrap(),
                &untrusted,
                &openssl::stack::Stack::new().unwrap(),
                |context| context.verify_cert()
            )
            .unwrap());
        let id = "001788fffe000001";
        let pin = "a".repeat(64);
        assert!(check_identity(
            id,
            &pin,
            BridgeTrust {
                bridge_id: None,
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            }
        )
        .is_ok());
        assert!(check_identity(
            id,
            "new",
            BridgeTrust {
                bridge_id: Some(id),
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            }
        )
        .is_ok());
        assert!(check_identity(
            id,
            "new",
            BridgeTrust {
                bridge_id: None,
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            }
        )
        .is_err());
        assert!(check_identity(
            id,
            &pin,
            BridgeTrust {
                bridge_id: Some("001788fffe000002"),
                certificate_sha256: Some(&pin),
                kind: BridgeKind::Real,
            }
        )
        .is_err());
        assert!(check_identity("not-a-bridge-id!!", &pin, BridgeTrust::default()).is_err());
    }

    #[test]
    fn unreachable_configured_bridge_keeps_the_tcp_error_as_root_cause() {
        use std::io::{Error as IoError, ErrorKind};
        let connect = IoError::new(ErrorKind::TimedOut, "connection timed out");
        let rediscovery = anyhow!("request failed").context("Hue cloud discovery failed");
        let error = unreachable_bridge("192.0.2.1", connect, rediscovery);
        let root = error.root_cause().downcast_ref::<IoError>().unwrap();
        assert_eq!(root.kind(), ErrorKind::TimedOut);
        let message = format!("{error:#}");
        assert!(message.contains("bridge IP 192.0.2.1 was unreachable"));
        assert!(message.contains("Hue cloud discovery failed: request failed"));
        assert!(message.ends_with(": connection timed out"), "{message}");
    }
}
