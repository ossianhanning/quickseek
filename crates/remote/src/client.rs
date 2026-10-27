//! The PC's side of a connection: hello, pairing or authentication.

use std::fmt;
use std::io;
use std::time::Duration;

use crate::pair;
use crate::tls::{self, ClientStream};
use crate::wire::{self, Msg, ShareInfo};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The agent presented another certificate than the pinned one.
    PinMismatch,
    /// The agent turned us down (not paired, wrong code, versions, ...).
    Refused(String),
    /// The agent's pairing answer didn't prove it knows the code.
    BadProof,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::PinMismatch => f.write_str(tls::PIN_MISMATCH),
            Error::Refused(s) => write!(f, "the agent refused: {s}"),
            Error::BadProof => f.write_str(
                "the agent couldn't prove it knows the pairing code (someone in between?)",
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        if e.kind() == io::ErrorKind::PermissionDenied && e.to_string() == tls::PIN_MISMATCH {
            Error::PinMismatch
        } else {
            Error::Io(e)
        }
    }
}

pub struct Session {
    pub stream: ClientStream,
    pub version: u8,
    pub build: String,
    /// The agent's name (its host name by default).
    pub agent: String,
    pub shares: Vec<ShareInfo>,
}

/// What pairing gives the PC to keep.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub fingerprint: [u8; 32],
    pub token: [u8; 32],
    pub agent: String,
    pub shares: Vec<ShareInfo>,
}

fn expect_ok(m: Msg) -> Result<Msg, Error> {
    match m {
        Msg::Error(e) => Err(Error::Refused(e)),
        m => Ok(m),
    }
}

fn hello(s: &mut ClientStream, build: &str) -> Result<(u8, String), Error> {
    wire::send(
        s,
        &Msg::Hello {
            min: crate::MIN_PROTOCOL,
            max: crate::PROTOCOL,
            build: build.into(),
        },
    )?;
    match expect_ok(wire::recv(s)?)? {
        Msg::Hello { max, build, .. } => Ok((max, build)),
        _ => Err(Error::Refused("unexpected answer to hello".into())),
    }
}

fn welcome(mut s: ClientStream, version: u8, build: String) -> Result<Session, Error> {
    match expect_ok(wire::recv(&mut s)?)? {
        Msg::Welcome { agent, shares } => Ok(Session {
            stream: s,
            version,
            build,
            agent,
            shares,
        }),
        _ => Err(Error::Refused("expected a welcome".into())),
    }
}

/// Connects to a paired agent.
pub fn connect(
    host: &str,
    port: u16,
    pin: [u8; 32],
    token: [u8; 32],
    pc_name: &str,
    build: &str,
    timeout: Duration,
) -> Result<Session, Error> {
    let (mut s, _) = tls::connect(host, port, Some(pin), timeout)?;
    let (version, agent_build) = hello(&mut s, build)?;
    wire::send(
        &mut s,
        &Msg::Auth {
            token,
            name: pc_name.into(),
        },
    )?;
    welcome(s, version, agent_build)
}

/// Pairs with an agent using the code it showed.
pub fn pair(
    host: &str,
    port: u16,
    code: &str,
    pc_name: &str,
    build: &str,
    timeout: Duration,
) -> Result<Pairing, Error> {
    let (mut s, fingerprint) = tls::connect(host, port, None, timeout)?;
    let (version, agent_build) = hello(&mut s, build)?;
    let nonce: [u8; 16] = pair::random();
    wire::send(
        &mut s,
        &Msg::Pair {
            proof: pair::pc_proof(code, &fingerprint, &nonce),
            nonce,
            name: pc_name.into(),
        },
    )?;
    let token = match expect_ok(wire::recv(&mut s)?)? {
        Msg::Paired { token, proof } => {
            if !pair::same(&proof, &pair::agent_proof(code, &fingerprint, &nonce)) {
                return Err(Error::BadProof);
            }
            token
        }
        _ => return Err(Error::Refused("expected a pairing answer".into())),
    };
    let sess = welcome(s, version, agent_build)?;
    Ok(Pairing {
        fingerprint,
        token,
        agent: sess.agent,
        shares: sess.shares,
    })
}

impl Session {
    pub fn send(&mut self, m: &Msg) -> Result<(), Error> {
        Ok(wire::send(&mut self.stream, m)?)
    }

    pub fn recv(&mut self) -> Result<Msg, Error> {
        expect_ok(wire::recv(&mut self.stream)?)
    }

    pub fn set_read_timeout(&mut self, t: Duration) -> io::Result<()> {
        self.stream.sock.set_read_timeout(Some(t))
    }
}
