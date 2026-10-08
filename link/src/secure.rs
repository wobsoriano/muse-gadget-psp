//! TLS 1.3 over any [`Stream`], with the server checked against the roots
//! this crate carries.

use crate::{Lost, Stream, Wire};
use alloc::{sync::Arc, vec, vec::Vec};
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{ClientConfig, RootCertStore};

/// The certificate authorities trusted, as DER. DigiCert Global Root G2 signs
/// api.muse.ai and the Muse hosts under metaaivm.com.
/// GTS Root R4 signs api.openai.com.
const ROOTS: [&[u8]; 2] =
    [include_bytes!("../certs/digicert_global_root_g2.der"), include_bytes!("../certs/gts_root_r4.der")];

const LARGEST_RECORD: usize = 16384 + 512;
const LARGEST_INCOMING: usize = 4 * LARGEST_RECORD;
const LARGEST_WRITE: usize = 8192;

#[derive(Debug)]
pub enum Error {
    Lost,
    /// The server ended the connection.
    Closed,
    /// The server sent more than this client will hold at once.
    Flooded,
    Tls(rustls::Error),
}

impl From<Lost> for Error {
    fn from(_: Lost) -> Self {
        Error::Lost
    }
}

impl From<rustls::Error> for Error {
    fn from(error: rustls::Error) -> Self {
        Error::Tls(error)
    }
}

/// What every connection shares. `clock` supplies the date certificates are
/// checked against. Randomness comes from the `getrandom` crate, so a machine
/// without a system source registers its own.
pub fn settings(clock: Arc<dyn TimeProvider>) -> Result<Arc<ClientConfig>, Error> {
    let mut roots = RootCertStore::empty();
    for root in ROOTS {
        roots.add(CertificateDer::from(root))?;
    }
    let config = ClientConfig::builder_with_details(Arc::new(rustls_rustcrypto::provider()), clock)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

enum Turn {
    Progressed,
    /// The handshake is done and nothing is waiting to be read.
    Idle,
    Sent,
    Closed,
}

pub struct Secure<S> {
    tls: UnbufferedClientConnection,
    stream: S,
    incoming: Vec<u8>,
    filled: usize,
    outgoing: Vec<u8>,
    plain: Vec<u8>,
    taken: usize,
}

impl<S: Stream> Secure<S> {
    /// Runs the handshake with `host` over `stream`.
    pub fn open(stream: S, host: &str, settings: Arc<ClientConfig>) -> Result<Self, Error> {
        let name = ServerName::try_from(host)
            .map_err(|_| Error::Tls(rustls::Error::General("not a host name".into())))?
            .to_owned();
        let mut secure = Secure {
            tls: UnbufferedClientConnection::new(settings, name)?,
            stream,
            incoming: vec![0; LARGEST_RECORD],
            filled: 0,
            outgoing: vec![0; LARGEST_RECORD],
            plain: Vec::new(),
            taken: 0,
        };
        loop {
            match secure.turn(None)? {
                Turn::Idle => return Ok(secure),
                Turn::Closed => return Err(Error::Closed),
                Turn::Progressed | Turn::Sent => {}
            }
        }
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        for chunk in bytes.chunks(LARGEST_WRITE) {
            loop {
                match self.turn(Some(chunk))? {
                    Turn::Sent => break,
                    Turn::Closed => return Err(Error::Closed),
                    Turn::Progressed | Turn::Idle => {}
                }
            }
        }
        Ok(())
    }

    /// Reads up to `into.len()` bytes. Zero means the server closed.
    pub fn read(&mut self, into: &mut [u8]) -> Result<usize, Error> {
        while self.taken == self.plain.len() {
            match self.turn(None)? {
                Turn::Closed => return Ok(0),
                Turn::Idle if self.receive()? == 0 => return Ok(0),
                Turn::Idle | Turn::Progressed | Turn::Sent => {}
            }
        }
        Ok(self.take(into))
    }

    fn take(&mut self, into: &mut [u8]) -> usize {
        let count = into.len().min(self.plain.len() - self.taken);
        into[..count].copy_from_slice(&self.plain[self.taken..self.taken + count]);
        self.taken += count;
        if self.taken == self.plain.len() {
            self.plain.clear();
            self.taken = 0;
        }
        count
    }

    /// Like `read`, but `None` when nothing arrives within `wait_ms`, so a
    /// caller with other work can come back later.
    pub fn read_soon(&mut self, into: &mut [u8], wait_ms: u32) -> Result<Option<usize>, Error>
    where
        S: Wire,
    {
        // Only the first wait is `wait_ms` long, so bytes that trickle in
        // cannot hold the caller past it.
        let mut wait_ms = wait_ms;
        while self.taken == self.plain.len() {
            match self.turn(None)? {
                Turn::Closed => return Ok(Some(0)),
                Turn::Idle => {
                    if !self.stream.readable(wait_ms)? {
                        return Ok(None);
                    }
                    if self.receive()? == 0 {
                        return Ok(Some(0));
                    }
                    wait_ms = 0;
                }
                Turn::Progressed | Turn::Sent => {}
            }
        }
        Ok(Some(self.take(into)))
    }

    fn receive(&mut self) -> Result<usize, Error> {
        if self.filled == self.incoming.len() {
            if self.incoming.len() >= LARGEST_INCOMING {
                return Err(Error::Flooded);
            }
            self.incoming.resize(self.incoming.len() + LARGEST_RECORD, 0);
        }
        let got = self.stream.read(&mut self.incoming[self.filled..])?;
        self.filled += got;
        Ok(got)
    }

    /// One step of the connection: whatever TLS needs next, plus `send` once
    /// the connection can carry it.
    fn turn(&mut self, send: Option<&[u8]>) -> Result<Turn, Error> {
        let UnbufferedStatus { mut discard, state } =
            self.tls.process_tls_records(&mut self.incoming[..self.filled]);
        let mut starved = false;
        let turn = match state? {
            ConnectionState::EncodeTlsData(mut pending) => {
                let length = match pending.encode(&mut self.outgoing) {
                    Err(EncodeError::InsufficientSize(needed)) => {
                        self.outgoing.resize(needed.required_size, 0);
                        pending.encode(&mut self.outgoing)
                    }
                    other => other,
                }
                .map_err(|_| Error::Flooded)?;
                self.stream.write_all(&self.outgoing[..length])?;
                Turn::Progressed
            }
            ConnectionState::TransmitTlsData(sent) => {
                sent.done();
                Turn::Progressed
            }
            ConnectionState::BlockedHandshake => {
                starved = true;
                Turn::Progressed
            }
            ConnectionState::ReadTraffic(mut records) => {
                while let Some(record) = records.next_record() {
                    let record = record?;
                    discard += record.discard;
                    self.plain.extend_from_slice(record.payload);
                }
                Turn::Progressed
            }
            ConnectionState::WriteTraffic(mut ready) => match send {
                None => Turn::Idle,
                Some(bytes) => {
                    let length = match ready.encrypt(bytes, &mut self.outgoing) {
                        Err(EncryptError::InsufficientSize(needed)) => {
                            self.outgoing.resize(needed.required_size, 0);
                            ready.encrypt(bytes, &mut self.outgoing)
                        }
                        other => other,
                    }
                    .map_err(|_| Error::Flooded)?;
                    self.stream.write_all(&self.outgoing[..length])?;
                    Turn::Sent
                }
            },
            ConnectionState::PeerClosed | ConnectionState::Closed => Turn::Closed,
            _ => return Err(Error::Tls(rustls::Error::General("a state this client never asks for".into()))),
        };
        self.incoming.copy_within(discard..self.filled, 0);
        self.filled -= discard;
        if starved && self.receive()? == 0 {
            return Ok(Turn::Closed);
        }
        Ok(turn)
    }
}
