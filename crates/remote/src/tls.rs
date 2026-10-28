//! TLS 1.3 without a CA: the agent presents a self-signed certificate and
//! the PC accepts exactly the one whose SHA-256 it pinned when pairing.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConnection, DigitallySignedStruct, ServerConnection, SignatureScheme};

pub type ClientStream = rustls::StreamOwned<ClientConnection, TcpStream>;
pub type ServerStream = rustls::StreamOwned<ServerConnection, TcpStream>;

/// Name the agent's certificate is issued to (the PC doesn't check names:
/// the pin is the identity).
pub const SERVER_NAME: &str = "quickseek-agent";

/// Text of the error when the agent presents a different certificate.
pub const PIN_MISMATCH: &str = "the agent's certificate changed (pair again if that's expected)";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    crate::pair::sha256(cert_der)
}

#[derive(Debug)]
struct Pinned {
    expect: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        *self.seen.lock().unwrap() = Some(fp);
        match self.expect {
            Some(want) if !crate::pair::same(&want, &fp) => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("only TLS 1.3 is used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The handshake must still be signed by the pinned certificate's key.
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Connects to an agent. With `pin`, any other certificate fails with
/// [`PIN_MISMATCH`]; without (pairing), any certificate is accepted and its
/// fingerprint returned so the pairing proof can bind it.
pub fn connect(
    host: &str,
    port: u16,
    pin: Option<[u8; 32]>,
    timeout: Duration,
) -> io::Result<(ClientStream, [u8; 32])> {
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address"));
    let mut sock = None;
    for a in &addrs {
        match TcpStream::connect_timeout(a, timeout) {
            Ok(s) => {
                sock = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let mut sock = sock.ok_or(last)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    let p = provider();
    let seen = Arc::new(Mutex::new(None));
    let verifier = Pinned {
        expect: pin,
        seen: seen.clone(),
        algs: p.signature_verification_algorithms,
    };
    let cfg = rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let mut conn = ClientConnection::new(Arc::new(cfg), ServerName::try_from(SERVER_NAME).unwrap())
        .map_err(io::Error::other)?;
    while conn.is_handshaking() {
        if let Err(e) = conn.complete_io(&mut sock) {
            let got = *seen.lock().unwrap();
            if let (Some(want), Some(got)) = (pin, got)
                && want != got
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    PIN_MISMATCH,
                ));
            }
            return Err(e);
        }
    }
    let fp = seen
        .lock()
        .unwrap()
        .ok_or_else(|| io::Error::other("no certificate"))?;
    Ok((rustls::StreamOwned::new(conn, sock), fp))
}

/// The agent's side: its certificate and PKCS#8 key, both DER.
pub fn server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> io::Result<Arc<rustls::ServerConfig>> {
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        )
        .map_err(io::Error::other)?;
    Ok(Arc::new(cfg))
}

/// Completes the server handshake on an accepted socket.
pub fn accept(
    cfg: Arc<rustls::ServerConfig>,
    mut sock: TcpStream,
    timeout: Duration,
) -> io::Result<ServerStream> {
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    let mut conn = ServerConnection::new(cfg).map_err(io::Error::other)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)?;
    }
    Ok(rustls::StreamOwned::new(conn, sock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Msg, recv, send};

    fn cert() -> (Vec<u8>, Vec<u8>) {
        let c = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()]).unwrap();
        (c.cert.der().to_vec(), c.signing_key.serialize_der())
    }

    /// Serves one connection per expected client, echoing one message back.
    fn serve(n: usize) -> (u16, [u8; 32]) {
        let (c, k) = cert();
        let fp = fingerprint(&c);
        let cfg = server_config(c, k).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().take(n) {
                let Ok(mut t) = accept(cfg.clone(), s.unwrap(), Duration::from_secs(5)) else {
                    continue;
                };
                if let Ok(m) = recv(&mut t) {
                    let _ = send(&mut t, &m);
                }
            }
        });
        (port, fp)
    }

    #[test]
    fn pinned_connection_works_and_other_pins_fail() {
        let (port, fp) = serve(3);
        let t = Duration::from_secs(5);
        // Pairing: no pin, learns the fingerprint.
        let (mut s, seen) = connect("127.0.0.1", port, None, t).unwrap();
        assert_eq!(seen, fp);
        send(&mut s, &Msg::Ping).unwrap();
        assert_eq!(recv(&mut s).unwrap(), Msg::Ping);
        // Pinned to the right certificate.
        let (mut s, _) = connect("127.0.0.1", port, Some(fp), t).unwrap();
        send(&mut s, &Msg::State("x".into())).unwrap();
        assert_eq!(recv(&mut s).unwrap(), Msg::State("x".into()));
        // Pinned to another one.
        let e = connect("127.0.0.1", port, Some([0; 32]), t).err().unwrap();
        assert_eq!(e.to_string(), PIN_MISMATCH);
    }
}
