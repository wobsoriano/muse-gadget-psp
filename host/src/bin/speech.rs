//! Speaks a sentence through the PSP's speech code on a computer and saves
//! what comes back.
//!
//! Usage: speech KEY_FILE OUTPUT.wav [TEXT]

use muse_link::{secure, speech, Lost, Net, Stream, Wire};
use rustls::pki_types::UnixTime;
use rustls::time_provider::TimeProvider;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct Clock;

impl TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(SystemTime::now().duration_since(UNIX_EPOCH).ok()?))
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

impl Wire for Tcp {
    fn readable(&mut self, _: u32) -> Result<bool, Lost> {
        Ok(true)
    }
}

struct Internet;

impl Net for Internet {
    type Wire = Tcp;

    fn open(&mut self, host: &str, port: u16) -> Result<Tcp, Lost> {
        let address = (host, port).to_socket_addrs().map_err(|_| Lost)?.find(|address| address.is_ipv4()).ok_or(Lost)?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(15)).map_err(|_| Lost)?;
        stream.set_read_timeout(Some(Duration::from_secs(15))).map_err(|_| Lost)?;
        Ok(Tcp(stream))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, key_file, output, rest @ ..] = args.as_slice() else {
        eprintln!("usage: speech KEY_FILE OUTPUT.wav [TEXT]");
        std::process::exit(2);
    };
    let key = speech::key_from(&std::fs::read(key_file).expect("a key file")).expect("a key in the file");
    let text = rest.first().map_or("Hello from the PSP, in Rust.", String::as_str);
    let settings = secure::settings(Arc::new(Clock)).expect("settings");
    let started = Instant::now();
    let (mut audio, mut pieces, mut first) = (Vec::new(), 0, None);
    let outcome = speech::speak(&mut Internet, settings, &key, "alloy", text, &mut |piece| {
        first.get_or_insert(started.elapsed());
        pieces += 1;
        audio.extend_from_slice(piece);
    });
    println!("{outcome:?}: {} bytes in {pieces} pieces, first after {:?}, all after {:?}", audio.len(), first, started.elapsed());
    println!("that is {:.1} s of sound", audio.len() as f32 / 2.0 / speech::RATE as f32);
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + audio.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    for field in [16, 0x0001_0001, speech::RATE, speech::RATE * 2, 0x0010_0002] {
        wav.extend_from_slice(&field.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(audio.len() as u32).to_le_bytes());
    wav.extend_from_slice(&audio);
    std::fs::write(output, wav).expect("the output file");
    if outcome.is_err() {
        std::process::exit(1);
    }
}
