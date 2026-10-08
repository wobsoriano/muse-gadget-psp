//! Runs the Muse client against tests/fake_muse.py over real sockets, from
//! one thread, the way the PSP app drives it.
//!
//! Usage: session API_PORT VM_PORT STATE_DIR [--voice] [--save-fails=N] [--until-unreachable] [--tls=CA_DER]
//!
//! With `--tls` every connection is TLS to "localhost", checked against the
//! one authority in CA_DER. Without it they are plaintext.
//!
//! Prints what the client reported as one JSON array on the last line.

use muse_link::{App, Client, Clock, Config, Event, Lost, Net, NotSaved, Pairing, Poll, Refused, Security, State, Stream, Vault, Wire};
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::{ClientConfig, RootCertStore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WAV_BYTES: usize = 100 * 1024;
const ASK_ATTEMPTS: u32 = 4;
const LONGEST_RUN: Duration = Duration::from_secs(150);

type Events = Rc<RefCell<Vec<Value>>>;

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
    fn readable(&mut self, wait_ms: u32) -> Result<bool, Lost> {
        // A zero timeout means "forever" to the socket, so one millisecond
        // stands in for not waiting at all.
        let wait = Duration::from_millis(u64::from(wait_ms.max(1)));
        self.0.set_read_timeout(Some(wait)).map_err(|_| Lost)?;
        let peeked = self.0.peek(&mut [0u8; 1]);
        self.0.set_read_timeout(Some(Duration::from_secs(15))).map_err(|_| Lost)?;
        match peeked {
            Ok(_) => Ok(true),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(false),
            Err(_) => Err(Lost),
        }
    }
}

struct Sockets;

impl Net for Sockets {
    type Wire = Tcp;

    fn open(&mut self, host: &str, port: u16) -> Result<Tcp, Lost> {
        let stream = TcpStream::connect((host, port)).map_err(|_| Lost)?;
        stream.set_nodelay(true).map_err(|_| Lost)?;
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(|_| Lost)?;
        Ok(Tcp(stream))
    }
}

struct Wall(Instant);

impl Clock for Wall {
    fn millis(&self) -> u64 {
        self.0.elapsed().as_millis() as u64
    }

    fn unix_seconds(&self) -> Option<u64> {
        Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
    }
}

#[derive(Debug)]
struct Today;

impl TimeProvider for Today {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(SystemTime::now().duration_since(UNIX_EPOCH).ok()?))
    }
}

/// What `secure::settings` builds, trusting the fake's authority in place of
/// the real one.
fn trusting(authority_der: &str) -> Security {
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(std::fs::read(authority_der).unwrap())).unwrap();
    let config = ClientConfig::builder_with_details(Arc::new(rustls_rustcrypto::provider()), Arc::new(Today))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Security::Tls(Arc::new(config))
}

/// Keeps the pairing the way the PSP app does: written beside the old file,
/// forced to disk, then renamed over it.
struct Files {
    dir: PathBuf,
    failures_left: u32,
    events: Events,
}

impl Vault for Files {
    fn save(&mut self, pairing: &Pairing) -> Result<(), NotSaved> {
        if self.failures_left > 0 {
            self.failures_left -= 1;
            self.events.borrow_mut().push(json!(["save_failed"]));
            return Err(NotSaved);
        }
        let text = json!({"access_token": pairing.access_token, "refresh_token": pairing.refresh_token, "saved_at": pairing.saved_at});
        let (new, kept) = (self.dir.join("pairing.json.new"), self.dir.join("pairing.json"));
        let written = std::fs::File::create(&new)
            .and_then(|mut file| file.write_all(text.to_string().as_bytes()).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&new, &kept));
        written.map_err(|_| NotSaved)?;
        self.events.borrow_mut().push(json!(["tokens", pairing.access_token, pairing.refresh_token]));
        Ok(())
    }
}

#[derive(Default)]
struct Driver {
    events: Events,
    state: Option<State>,
    asks_done: u32,
    /// The reply as last reported before the question ended.
    partial: String,
    /// How the question in flight ended: true for done.
    ended: Option<bool>,
}

impl Driver {
    fn record(&self, event: Value) {
        self.events.borrow_mut().push(event);
    }
}

impl App for Driver {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::State(state) => {
                self.state = Some(state);
                self.record(match state {
                    State::Connecting => json!(["state", "connecting"]),
                    State::Connected => json!(["state", "connected"]),
                    State::Unpaired => json!(["state", "unpaired"]),
                    State::Unreachable(trouble) => json!(["state", "unreachable", format!("{trouble:?}")]),
                });
            }
            Event::Log(line) => self.record(json!(["log", line.to_string()])),
            Event::Heard(text) => self.record(json!(["heard", text])),
            Event::Reply(text) => self.partial = text.into(),
            Event::Settled(text) => self.record(json!(["settled", text])),
            Event::Done(text) => {
                self.record(json!(["ask", text, self.partial]));
                self.asks_done += 1;
                self.ended = Some(true);
            }
            Event::Failed { text, why } => {
                self.record(json!(["ask_failed", why, text]));
                self.ended = Some(false);
            }
        }
    }

    fn invoke(&mut self, command: &str, params: &str) -> Result<String, String> {
        if command == "test.progress" {
            return Ok(json!({"asks_done": self.asks_done}).to_string());
        }
        let params: Value = serde_json::from_str(params).map_err(|error| error.to_string())?;
        let text = params["text"].as_str().unwrap_or_default();
        self.record(json!(["invoke", command, text.len()]));
        match command {
            "echo" => Ok(json!({"text": text}).to_string()),
            "badge.show_message" => Ok(json!({"shown": text}).to_string()),
            _ => Err(format!("unsupported command: {command}")),
        }
    }
}

/// Sixteen samples of one sine period, repeated behind a WAV header.
fn make_wav() -> Vec<u8> {
    const PERIOD: [i16; 16] = [0, 4592, 8485, 11087, 12000, 11087, 8485, 4592, 0, -4592, -8485, -11087, -12000, -11087, -8485, -4592];
    let mut wav = Vec::with_capacity(WAV_BYTES);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(WAV_BYTES as u32 - 8).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&[16, 0, 0, 0, 1, 0, 1, 0, 0x80, 0x3E, 0, 0, 0x00, 0x7D, 0, 0, 2, 0, 16, 0]);
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(WAV_BYTES as u32 - 44).to_le_bytes());
    wav.extend(PERIOD.iter().cycle().take((WAV_BYTES - 44) / 2).flat_map(|sample| sample.to_le_bytes()));
    wav
}

enum Question {
    Text(&'static str),
    Voice,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [api_port, vm_port, state_dir, flags @ ..] = args.as_slice() else {
        eprintln!("usage: session API_PORT VM_PORT STATE_DIR [--voice] [--save-fails=N] [--until-unreachable] [--tls=CA_DER]");
        std::process::exit(2);
    };
    let flag = |name: &str| flags.iter().any(|flag| flag == name);
    let save_fails = flags.iter().find_map(|flag| flag.strip_prefix("--save-fails=")).map_or(0, |count| count.parse().unwrap());

    let mut questions = vec![Question::Text("what is up"), Question::Text("and now")];
    if flag("--voice") {
        questions.push(Question::Voice);
    }
    let mut questions = questions.into_iter().peekable();

    let mut config = Config::new("homelink-010203", "Test Badge", "0.1.0", "musebadge-test");
    let authority = flags.iter().find_map(|flag| flag.strip_prefix("--tls="));
    let (security, scheme, host) = match authority {
        Some(authority) => (trusting(authority), "s", "localhost"),
        None => (Security::PlaintextForTests, "", "127.0.0.1"),
    };
    config.api_root = Some(format!("http{scheme}://{host}:{api_port}"));
    config.noise_host = Some(format!("ws{scheme}://{host}:{vm_port}"));
    config.sdk_token = Some("mgst_test".into());
    config.commands_json = Some(r#"{"badge.show_message":{}}"#.into());
    config.network_ssid = Some("TestNet".into());
    let pairing = Pairing { access_token: "old-access".into(), refresh_token: "hatch_refresh:r1".into(), saved_at: 0 };

    let events = Events::default();
    let vault = Files { dir: state_dir.into(), failures_left: save_fails, events: events.clone() };
    let mut driver = Driver { events: events.clone(), ..Driver::default() };
    let started = Instant::now();
    let mut client = Client::new(config, pairing, security, Sockets, Wall(started), vault).expect("a usable config");
    driver.event(Event::State(client.state()));

    let mut attempts = 0;
    let mut waiting = false;
    let ok = loop {
        if started.elapsed() > LONGEST_RUN {
            break false;
        }
        match client.poll(&mut driver, 20) {
            Poll::Unpaired => break questions.peek().is_none(),
            Poll::Resting(ms) => std::thread::sleep(Duration::from_millis(u64::from(ms.min(50)))),
            Poll::Active => {}
        }
        if flag("--until-unreachable") && matches!(driver.state, Some(State::Unreachable(_))) {
            break true;
        }
        if waiting {
            match driver.ended.take() {
                Some(true) => {
                    questions.next();
                    (waiting, attempts) = (false, 0);
                }
                // A question that failed because the session dropped is asked
                // again on the next session.
                Some(false) if attempts < ASK_ATTEMPTS => waiting = false,
                Some(false) => break false,
                None => {}
            }
        } else if let Some(question) = questions.peek() {
            let asked = match question {
                Question::Text(text) => client.ask_text(text),
                Question::Voice => {
                    let wav = make_wav();
                    if attempts == 0 {
                        let digest: String = Sha256::digest(&wav).iter().map(|byte| format!("{byte:02x}")).collect();
                        driver.record(json!(["voice", wav.len(), digest]));
                    }
                    client.ask_voice(wav)
                }
            };
            match asked {
                Ok(()) => {
                    driver.partial.clear();
                    (waiting, attempts) = (true, attempts + 1);
                }
                Err(Refused::Offline) => {}
                Err(refused) => {
                    driver.record(json!(["refused", format!("{refused:?}")]));
                    break false;
                }
            }
        }
    };
    println!("{}", Value::Array(events.take()));
    std::process::exit(if ok { 0 } else { 1 });
}
