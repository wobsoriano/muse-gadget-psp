//! One HTTPS request over any [`Stream`].

use crate::secure::{Error, Secure};
use crate::Stream;
use alloc::{format, sync::Arc, vec, vec::Vec};
use rustls::ClientConfig;

/// How far a request got. `get` reports each one as it is reached, so a
/// caller can time them and a stall can be placed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    Secured,
    Asked,
    Answered,
}

/// Asks `host` for `path` and returns the start of what it sends back.
pub fn get<S: Stream>(
    stream: S,
    host: &str,
    path: &str,
    settings: Arc<ClientConfig>,
    reached: &mut dyn FnMut(Stage),
) -> Result<Vec<u8>, Error> {
    let mut secure = Secure::open(stream, host, settings)?;
    reached(Stage::Secured);
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    secure.write_all(request.as_bytes())?;
    reached(Stage::Asked);
    let mut head = vec![0u8; 512];
    let got = secure.read(&mut head)?;
    head.truncate(got);
    reached(Stage::Answered);
    Ok(head)
}
