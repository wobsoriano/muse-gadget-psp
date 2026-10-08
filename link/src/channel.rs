//! The connection a request travels on. Outside of tests that is always TLS
//! with the server's certificate checked.

use crate::secure::{self, Secure};
use crate::url::Endpoint;
use crate::{Lost, Net, Wire};
use alloc::sync::Arc;
use rustls::ClientConfig;

/// How connections are protected.
#[derive(Clone)]
pub enum Security {
    /// TLS with certificate checks, from [`secure::settings`]. Addresses must
    /// be https or wss.
    Tls(Arc<ClientConfig>),
    /// No protection at all, for a fake server on the same machine. Addresses
    /// must be http or ws, so this can never carry a token to a real host
    /// that expects TLS.
    #[cfg(feature = "plaintext-for-tests")]
    PlaintextForTests,
}

impl Security {
    pub(crate) fn fits(&self, to: &Endpoint) -> bool {
        match self {
            Security::Tls(_) => to.secure,
            #[cfg(feature = "plaintext-for-tests")]
            Security::PlaintextForTests => !to.secure,
        }
    }
}

/// Why a step of the connection failed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Fault {
    Lost,
    Closed,
    /// TLS failed, which includes a certificate that did not check out.
    Security,
    Protocol,
    TooBig,
    Timeout,
    /// The far side answered with this HTTP status.
    Refused(u16),
    /// Pings drew no answer.
    Quiet,
}

impl From<Lost> for Fault {
    fn from(_: Lost) -> Self {
        Fault::Lost
    }
}

impl From<secure::Error> for Fault {
    fn from(error: secure::Error) -> Self {
        match error {
            secure::Error::Lost => Fault::Lost,
            secure::Error::Closed => Fault::Closed,
            secure::Error::Flooded => Fault::TooBig,
            secure::Error::Tls(_) => Fault::Security,
        }
    }
}

pub(crate) enum Channel<W> {
    Secure(Secure<W>),
    #[cfg(feature = "plaintext-for-tests")]
    Plain(W),
}

impl<W: Wire> Channel<W> {
    pub(crate) fn open(net: &mut impl Net<Wire = W>, security: &Security, to: &Endpoint) -> Result<Self, Fault> {
        if !security.fits(to) {
            return Err(Fault::Security);
        }
        let wire = net.open(&to.host, to.port)?;
        match security {
            Security::Tls(settings) => Ok(Channel::Secure(Secure::open(wire, &to.host, settings.clone())?)),
            #[cfg(feature = "plaintext-for-tests")]
            Security::PlaintextForTests => Ok(Channel::Plain(wire)),
        }
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        match self {
            Channel::Secure(secure) => Ok(secure.write_all(bytes)?),
            #[cfg(feature = "plaintext-for-tests")]
            Channel::Plain(wire) => Ok(wire.write_all(bytes)?),
        }
    }

    /// Reads what arrives within `wait_ms`. `None` is nothing yet, and zero
    /// is the far side closing.
    pub(crate) fn read_soon(&mut self, into: &mut [u8], wait_ms: u32) -> Result<Option<usize>, Fault> {
        match self {
            Channel::Secure(secure) => Ok(secure.read_soon(into, wait_ms)?),
            #[cfg(feature = "plaintext-for-tests")]
            Channel::Plain(wire) => Ok(if wire.readable(wait_ms)? { Some(wire.read(into)?) } else { None }),
        }
    }
}
