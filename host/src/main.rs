//! Runs the PSP's connection code on a computer against the real server.
//!
//! Usage: host-check [HOST] [--clock=UNIX_SECONDS] [--at=OTHER_HOST]
//!
//! `--clock` checks certificates against another date. `--at` connects to
//! OTHER_HOST's address while still asking for HOST. Both must fail.

use muse_link::{dns, https, secure, Lost, Stream};
use rustls::pki_types::UnixTime;
use rustls::time_provider::TimeProvider;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct Clock(Option<u64>);

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        let seconds = match self.0 {
            Some(forced) => forced,
            None => SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs(),
        };
        Some(UnixTime::since_unix_epoch(Duration::from_secs(seconds)))
    }
}

struct Tcp(TcpStream);

impl Stream for Tcp {
    fn read(&mut self, into: &mut [u8]) -> Result<usize, Lost> {
        self.0.read(into).map_err(|_| Lost)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Lost> {
        self.0.write_all(bytes).map_err(|_| Lost)
    }
}

fn lookup(host: &str) -> Ipv4Addr {
    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.set_read_timeout(Some(Duration::from_millis(1500))).unwrap();
    let mut packet = [0u8; 512];
    let n = dns::query(&mut packet, host, 0x4D55).expect("a name that fits");
    socket.send_to(&packet[..n], "1.1.1.1:53").unwrap();
    let (got, _) = socket.recv_from(&mut packet).expect("a DNS reply");
    Ipv4Addr::from(dns::answer(&packet[..got], 0x4D55).expect("an address in the reply"))
}

fn main() {
    let (mut host, mut at, mut clock) = (String::from("api.muse.ai"), None, None);
    for arg in std::env::args().skip(1) {
        if let Some(seconds) = arg.strip_prefix("--clock=") {
            clock = Some(seconds.parse().unwrap());
        } else if let Some(other) = arg.strip_prefix("--at=") {
            at = Some(other.to_string());
        } else {
            host = arg;
        }
    }
    let address = lookup(at.as_deref().unwrap_or(&host));
    println!("{host} at {address}");
    let stream = TcpStream::connect((address, 443)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let started = Instant::now();
    let reply = secure::settings(Arc::new(Clock(clock))).and_then(|settings| {
        https::get(Tcp(stream), &host, "/", settings, &mut |stage| {
            println!("{stage:?} after {} ms", started.elapsed().as_millis())
        })
    });
    match reply {
        Ok(head) => println!("{}", String::from_utf8_lossy(&head).lines().next().unwrap_or("(empty)")),
        Err(error) => {
            println!("FAILED: {error:?}");
            std::process::exit(1);
        }
    }
}
