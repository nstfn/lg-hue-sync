use anyhow::{anyhow, Context, Result};
use openssl::ssl::{HandshakeError, SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug)]
pub struct ConnectedUdpSocket {
    socket: UdpSocket,
    deadline: Option<Instant>,
}

impl ConnectedUdpSocket {
    fn timeout(&self) -> std::io::Result<Duration> {
        match self.deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "DTLS handshake deadline expired",
                    )
                }),
            None => Ok(Duration::from_secs(5)),
        }
    }
}

impl Read for ConnectedUdpSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.is_some() {
            self.socket.set_read_timeout(Some(self.timeout()?))?;
        }
        self.socket.recv(buf)
    }
}

impl Write for ConnectedUdpSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.deadline.is_some() {
            self.socket.set_write_timeout(Some(self.timeout()?))?;
        }
        self.socket.send(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct HueDtlsClient {
    stream: SslStream<ConnectedUdpSocket>,
}

/// Overall budget of one `connect` call, across all handshake attempts.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(5);
/// Budget of one handshake attempt. A lost datagram costs one attempt, not the whole deadline.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);
/// Pause between two attempts.
const RETRY_PAUSE: Duration = Duration::from_millis(200);
/// An attempt shorter than this cannot complete a handshake with the bridge, so it is not started.
const MIN_ATTEMPT_WINDOW: Duration = Duration::from_millis(250);
const MAX_HANDSHAKE_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptPlan {
    /// Sleep before the attempt starts.
    pause: Duration,
    /// Time the attempt may use once started.
    window: Duration,
}

/// Plans 1-based handshake attempt `attempt`, `elapsed` after `connect` started, or returns
/// `None` when no attempt may start. Attempts that all time out give 4 full 1 s windows inside
/// the 5 s deadline; attempts that fail fast (for example an ICMP port unreachable) allow 5.
fn plan_attempt(attempt: u32, elapsed: Duration) -> Option<AttemptPlan> {
    if attempt == 0 || attempt > MAX_HANDSHAKE_ATTEMPTS {
        return None;
    }
    let pause = if attempt == 1 {
        Duration::ZERO
    } else {
        RETRY_PAUSE
    };
    let remaining = HANDSHAKE_DEADLINE.checked_sub(elapsed.checked_add(pause)?)?;
    (remaining >= MIN_ATTEMPT_WINDOW).then(|| AttemptPlan {
        pause,
        window: remaining.min(ATTEMPT_TIMEOUT),
    })
}

/// Names the SSL result code, the IO error kind and the OpenSSL error stack of a failed
/// handshake. None of them carries the PSK identity or key.
fn describe_ssl_error(error: &openssl::ssl::Error) -> String {
    let mut text = format!("ssl_error_code={}", error.code().as_raw());
    if let Some(io) = error.io_error() {
        text.push_str(&format!(", io_error_kind={:?}, io_error={}", io.kind(), io));
    }
    if let Some(stack) = error.ssl_error() {
        text.push_str(&format!(", openssl_errors={}", stack));
    }
    text
}

/// Runs one handshake on its own socket until it succeeds, fails, or `deadline` passes.
fn handshake_once(
    connector: &SslConnector,
    socket: UdpSocket,
    deadline: Instant,
) -> std::result::Result<SslStream<ConnectedUdpSocket>, String> {
    let mut result = connector.connect(
        "HueBridge",
        ConnectedUdpSocket {
            socket,
            deadline: Some(deadline),
        },
    );
    loop {
        match result {
            Ok(stream) => return Ok(stream),
            // A read timeout surfaces as WouldBlock. Continue the same handshake: each re-entry
            // lets OpenSSL check its DTLS retransmission timer and resend the last flight, and a
            // late reply inside the attempt window still completes it. Once the window has
            // passed, ConnectedUdpSocket returns TimedOut, which ends the attempt as a Failure.
            Err(HandshakeError::WouldBlock(mid)) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "attempt window expired ({})",
                        describe_ssl_error(mid.error())
                    ));
                }
                result = mid.handshake();
            }
            Err(HandshakeError::Failure(mid)) => return Err(describe_ssl_error(mid.error())),
            Err(HandshakeError::SetupFailure(stack)) => {
                return Err(format!("setup failed: openssl_errors={}", stack))
            }
        }
    }
}

fn build_connector(application_id: &str, psk_bytes: [u8; 16]) -> Result<SslConnector> {
    let mut builder = SslConnector::builder(SslMethod::dtls())
        .with_context(|| "Failed to create DTLS SSL connector builder")?;

    builder.set_min_proto_version(Some(SslVersion::DTLS1_2))?;
    builder.set_max_proto_version(Some(SslVersion::DTLS1_2))?;
    // Hue supports DTLS 1.2 with this PSK cipher.
    builder
        .set_cipher_list("PSK-AES128-GCM-SHA256")
        .with_context(|| "Failed to set PSK cipher list")?;

    // Disable server certificate verification since Hue uses pre-shared keys
    builder.set_verify(SslVerifyMode::NONE);

    let identity = application_id.to_string();
    builder.set_psk_client_callback(move |_ssl, _hint, identity_buf, psk_buf| {
        let id_bytes = identity.as_bytes();
        if id_bytes.len() >= identity_buf.len() {
            return Err(openssl::error::ErrorStack::get());
        }
        identity_buf[..id_bytes.len()].copy_from_slice(id_bytes);
        identity_buf[id_bytes.len()] = 0;

        if psk_bytes.len() > psk_buf.len() {
            return Err(openssl::error::ErrorStack::get());
        }
        psk_buf[..psk_bytes.len()].copy_from_slice(&psk_bytes);

        Ok(psk_bytes.len())
    });

    Ok(builder.build())
}

impl HueDtlsClient {
    /// Connect to Hue Bridge DTLS port 2100 with PSK identity and client key
    pub fn connect(bridge_ip: &str, application_id: &str, clientkey_hex: &str) -> Result<Self> {
        if !super::valid_configuration_id(application_id) {
            return Err(anyhow!("Invalid Hue DTLS application ID"));
        }
        let psk_bytes = hex_to_bytes(clientkey_hex).with_context(|| {
            "Failed to parse clientkey as hex (must be 32 hex chars / 16 bytes)"
        })?;

        info!(
            "Configuring DTLS 1.2 PSK connector for {}:2100...",
            bridge_ip
        );

        let connector = build_connector(application_id, psk_bytes)?;

        let target_ip: IpAddr = bridge_ip
            .parse()
            .context("Hue Bridge address must be an IP literal")?;
        Self::connect_to(&connector, SocketAddr::new(target_ip, 2100))
    }

    fn connect_to(connector: &SslConnector, target: SocketAddr) -> Result<Self> {
        let started = Instant::now();
        let overall_deadline = started + HANDSHAKE_DEADLINE;
        let mut last_error = String::from("no attempt started");
        for attempt in 1..=MAX_HANDSHAKE_ATTEMPTS {
            let Some(plan) = plan_attempt(attempt, started.elapsed()) else {
                break;
            };
            std::thread::sleep(plan.pause);
            // A fresh socket, so on a new local port, for each attempt: late datagrams of an
            // earlier attempt cannot reach this one.
            let socket =
                UdpSocket::bind("0.0.0.0:0").with_context(|| "Failed to bind local UDP socket")?;
            socket
                .connect(target)
                .with_context(|| format!("Failed to connect UDP socket to {}", target))?;
            info!(
                "Performing DTLS handshake with {} (attempt {}/{}, {} ms)...",
                target,
                attempt,
                MAX_HANDSHAKE_ATTEMPTS,
                plan.window.as_millis()
            );
            let deadline = (Instant::now() + plan.window).min(overall_deadline);
            match handshake_once(connector, socket, deadline) {
                Ok(mut stream) => {
                    stream.get_mut().deadline = None;
                    stream
                        .get_ref()
                        .socket
                        .set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream
                        .get_ref()
                        .socket
                        .set_write_timeout(Some(Duration::from_millis(100)))?;
                    info!("[+] DTLS 1.2 handshake successful with Hue Bridge!");
                    return Ok(Self { stream });
                }
                Err(error) => {
                    tracing::warn!("DTLS handshake attempt {} failed: {}", attempt, error);
                    last_error = error;
                }
            }
        }

        Err(anyhow!(
            "DTLS handshake failed within retry/deadline limits: {}",
            last_error
        ))
    }

    /// Send a binary HueStream packet over the encrypted DTLS channel
    pub fn send(&mut self, packet: &[u8]) -> Result<()> {
        self.stream.write_all(packet)?;
        self.stream.flush()?;
        Ok(())
    }
}

fn hex_to_bytes(hex: &str) -> Result<[u8; 16]> {
    let hex = hex.trim();
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "Client key must contain exactly 32 ASCII hex characters"
        ));
    }
    let mut key = [0; 16];
    // ASCII validation above makes every pair a valid UTF-8 boundary.
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_keys_require_exact_ascii_hex_without_panicking() {
        for invalid in [
            "",
            "aéa",
            "éééééééééééééééé",
            "00",
            "0000000000000000000000000000000g",
        ] {
            assert!(hex_to_bytes(invalid).is_err());
        }
        assert_eq!(
            hex_to_bytes("00112233445566778899aAbBcCdDeEfF").unwrap(),
            [0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255]
        );
        assert!(HueDtlsClient::connect("127.0.0.1", "user", "aéa").is_err());
    }

    #[test]
    fn handshake_deadline_bounds_reads_and_expired_writes() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(peer.local_addr().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(30);
        let mut transport = ConnectedUdpSocket {
            socket,
            deadline: Some(deadline),
        };
        let error = transport.read(&mut [0; 1]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(Instant::now().duration_since(deadline) < Duration::from_secs(1));
        transport.deadline = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(
            transport.write(&[0]).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn schedule_gives_four_full_attempts_when_each_one_times_out() {
        assert_eq!(HANDSHAKE_DEADLINE, Duration::from_secs(5));
        assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(1));
        let mut elapsed = Duration::ZERO;
        let mut windows = Vec::new();
        for attempt in 1..=MAX_HANDSHAKE_ATTEMPTS + 1 {
            let Some(plan) = plan_attempt(attempt, elapsed) else {
                break;
            };
            elapsed += plan.pause + plan.window;
            windows.push(plan.window);
        }
        assert_eq!(windows, vec![ATTEMPT_TIMEOUT; 4]);
        assert!(elapsed <= HANDSHAKE_DEADLINE);
    }

    #[test]
    fn schedule_allows_five_attempts_when_attempts_fail_fast() {
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0;
        for attempt in 1..=MAX_HANDSHAKE_ATTEMPTS + 1 {
            let Some(plan) = plan_attempt(attempt, elapsed) else {
                break;
            };
            assert_eq!(plan.window, ATTEMPT_TIMEOUT);
            elapsed += plan.pause + Duration::from_millis(10);
            attempts += 1;
        }
        assert_eq!(attempts, MAX_HANDSHAKE_ATTEMPTS);
        assert!(elapsed < HANDSHAKE_DEADLINE);
    }

    #[test]
    fn schedule_respects_attempt_range_pause_and_overall_deadline() {
        assert_eq!(
            plan_attempt(1, Duration::ZERO),
            Some(AttemptPlan {
                pause: Duration::ZERO,
                window: ATTEMPT_TIMEOUT
            })
        );
        assert_eq!(plan_attempt(0, Duration::ZERO), None);
        assert_eq!(
            plan_attempt(MAX_HANDSHAKE_ATTEMPTS + 1, Duration::ZERO),
            None
        );
        assert_eq!(plan_attempt(1, HANDSHAKE_DEADLINE), None);
        assert_eq!(plan_attempt(1, Duration::from_secs(60)), None);
        // The last window is cut to the overall deadline.
        assert_eq!(
            plan_attempt(2, Duration::from_millis(4_400)),
            Some(AttemptPlan {
                pause: RETRY_PAUSE,
                window: Duration::from_millis(400)
            })
        );
        // 200 ms would be left after the pause: below the minimum window, so no attempt.
        assert_eq!(plan_attempt(2, Duration::from_millis(4_600)), None);
    }

    /// Server side of the fake bridge: replays the datagram already read, then uses the socket.
    #[derive(Debug)]
    struct PeerTransport {
        socket: UdpSocket,
        pending: Option<Vec<u8>>,
    }

    impl Read for PeerTransport {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(datagram) = self.pending.take() {
                let len = datagram.len().min(buf.len());
                buf[..len].copy_from_slice(&datagram[..len]);
                return Ok(len);
            }
            self.socket.recv(buf)
        }
    }

    impl Write for PeerTransport {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.socket.send(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn handshake_retries_on_a_fresh_socket_after_a_lost_datagram() {
        use openssl::ssl::{Ssl, SslContext};
        use std::sync::{Arc, Mutex};

        // Test-only values, never a real bridge credential.
        const APPLICATION_ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
        const PSK: [u8; 16] = [7; 16];

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            // Drop the first datagram: the first attempt's ClientHello is lost.
            let (_, first_source) = peer.recv_from(&mut buf).unwrap();
            let (len, second_source) = loop {
                let (len, source) = peer.recv_from(&mut buf).unwrap();
                if source != first_source {
                    break (len, source);
                }
            };
            peer.connect(second_source).unwrap();

            let mut context = SslContext::builder(SslMethod::dtls()).unwrap();
            context
                .set_min_proto_version(Some(SslVersion::DTLS1_2))
                .unwrap();
            context
                .set_max_proto_version(Some(SslVersion::DTLS1_2))
                .unwrap();
            context.set_cipher_list("PSK-AES128-GCM-SHA256").unwrap();
            let seen_identity = Arc::new(Mutex::new(None));
            let identity_slot = Arc::clone(&seen_identity);
            context.set_psk_server_callback(move |_ssl, identity, psk_buf| {
                *identity_slot.lock().unwrap() = identity.map(<[u8]>::to_vec);
                psk_buf[..PSK.len()].copy_from_slice(&PSK);
                Ok(PSK.len())
            });
            let ssl = Ssl::new(&context.build()).unwrap();
            let mut result = ssl.accept(PeerTransport {
                socket: peer,
                pending: Some(buf[..len].to_vec()),
            });
            let mut stream = loop {
                match result {
                    Ok(stream) => break stream,
                    Err(HandshakeError::WouldBlock(mid)) => result = mid.handshake(),
                    Err(error) => panic!("fake bridge handshake failed: {error}"),
                }
            };
            let mut packet = [0u8; 64];
            let len = stream.read(&mut packet).unwrap();
            let identity = seen_identity.lock().unwrap().clone();
            (
                first_source,
                second_source,
                identity,
                packet[..len].to_vec(),
            )
        });

        let connector = build_connector(APPLICATION_ID, PSK).unwrap();
        let started = Instant::now();
        let mut client = HueDtlsClient::connect_to(&connector, peer_addr).unwrap();
        let elapsed = started.elapsed();
        client.send(b"huestream").unwrap();
        let (first_source, second_source, identity, payload) = server.join().unwrap();

        assert_ne!(first_source.port(), second_source.port());
        assert_eq!(identity.as_deref(), Some(APPLICATION_ID.as_bytes()));
        assert_eq!(payload, b"huestream");
        // The lost datagram cost one attempt window, not the whole deadline.
        assert!(elapsed >= ATTEMPT_TIMEOUT, "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }
}
