//! Keeps a paired device connected to its Muse. Ported from the Muse Gadget
//! SDK's service.py: each round rotates the device token when due, asks the
//! API which VM to use, serves one session, then waits before the next.
//!
//! Nothing here runs by itself. The app calls [`Client::poll`] over and over
//! from one thread, and everything the client has to say comes back through
//! [`App`] during that call.

use crate::channel::{Fault, Security};
use crate::pace::{Backoff, Door, REFUSED_WAIT_MS};
use crate::session::{log, Ask, End, Hello, Session, LARGEST_WAV};
use crate::url::{encode_component, Endpoint};
use crate::{api, Clock, Net};
use alloc::{format, string::String, vec::Vec};
use core::fmt;
use serde_json::{json, Value};
use zeroize::Zeroize;

const DEFAULT_API_ROOT: &str = "https://api.muse.ai";
const DEFAULT_NOISE_HOST: &str = "hatch.metaaivm.com";
/// A session registered this long clears the backoff.
const HEALTHY_SESSION_MS: u64 = 30_000;
/// Device access tokens live about four hours.
const REFRESH_AGE_S: u64 = 3 * 3600;
const REFRESH_RETRY_MS: u64 = 300_000;
const LONGEST_QUESTION: usize = 16 * 1024;

/// The device's tokens. Refreshing them kills the pair that was used, so the
/// new pair is the only way back in from then on.
#[derive(Clone, PartialEq)]
pub struct Pairing {
    pub access_token: String,
    /// With or without its `hatch_refresh:` prefix.
    pub refresh_token: String,
    /// Seconds since 1970 when the access token was issued. Zero refreshes
    /// before first use.
    pub saved_at: u64,
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// Where the pairing is kept.
pub trait Vault {
    /// Stores the pairing in place of the last one. Must not return `Ok`
    /// until it would survive the power going out: the client uses the new
    /// tokens only after this, and the old ones are already dead.
    fn save(&mut self, pairing: &Pairing) -> Result<(), NotSaved>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NotSaved;

pub struct Config {
    /// Where the device API is. `None` is https://api.muse.ai.
    pub api_root: Option<String>,
    /// "host[:port]" or "wss://host[:port]". `None` is hatch.metaaivm.com.
    pub noise_host: Option<String>,
    pub node_id: String,
    pub display_name: String,
    pub version: String,
    pub user_agent: String,
    pub sdk_token: Option<String>,
    /// The commands this device runs, as the JSON object Muse calls
    /// commands_v2. `None` is no commands.
    pub commands_json: Option<String>,
    pub network_ssid: Option<String>,
}

impl Config {
    /// The real Muse addresses, no commands, and nothing optional.
    pub fn new(node_id: &str, display_name: &str, version: &str, user_agent: &str) -> Config {
        Config {
            api_root: None,
            noise_host: None,
            node_id: node_id.into(),
            display_name: display_name.into(),
            version: version.into(),
            user_agent: user_agent.into(),
            sdk_token: None,
            commands_json: None,
            network_ssid: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BadConfig {
    /// An address is not one this client may connect to. With
    /// [`Security::Tls`] that is anything but https and wss.
    Address,
    /// `commands_json` is not a JSON object.
    Commands,
    /// `node_id`, `display_name`, `version` or `user_agent` is empty.
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    /// Reaching Muse, and nothing has gone wrong since it last worked.
    Connecting,
    /// The last try failed. The client keeps trying, and this stays until
    /// one works.
    Unreachable(Trouble),
    /// Registered: commands arrive and questions can be asked.
    Connected,
    /// Muse removed this device or revoked its tokens. The client is done.
    Unpaired,
}

/// Why Muse could not be reached.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trouble {
    /// No connection, or one that went silent: Wi-Fi, name lookup, the
    /// network in between.
    Network,
    /// TLS failed. A certificate that does not check out lands here, which
    /// on a device with a wrong date is every certificate.
    Security,
    /// The API or the VM answered with this HTTP status.
    Refused(u16),
    /// The API answered, with no VM for this device.
    NoVm,
    /// The far side said something this client does not understand.
    Protocol,
    /// New tokens could not be saved. They are held and saved before
    /// anything else is tried.
    Storage,
}

impl From<Fault> for Trouble {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Lost | Fault::Closed | Fault::Timeout | Fault::Quiet => Trouble::Network,
            Fault::Security => Trouble::Security,
            Fault::Protocol | Fault::TooBig => Trouble::Protocol,
            Fault::Refused(status) => Trouble::Refused(status),
        }
    }
}

pub enum Event<'a> {
    State(State),
    /// A line for a log. Formatting it is the app's choice and cost.
    Log(fmt::Arguments<'a>),
    /// What Muse heard in the voice note just asked.
    Heard(&'a str),
    /// The reply so far, each time it grows.
    Reply(&'a str),
    /// Every reply message is complete and the stream has paused. This is
    /// almost always the whole answer, a couple of seconds before `Done`
    /// confirms it, so it is the moment to start anything slow. A later
    /// message can still extend the text.
    Settled(&'a str),
    /// The reply is complete. Ends the question.
    Done(&'a str),
    /// The question failed. `text` is whatever part of a reply had arrived.
    /// Ends the question.
    Failed { text: &'a str, why: &'a str },
}

/// The app's side of the client. Both are called only during
/// [`Client::poll`], on the thread that called it.
pub trait App {
    fn event(&mut self, event: Event<'_>);
    /// Runs a command Muse sent. `params` is a JSON object. `Ok` carries the
    /// result as JSON, `Err` a sentence for Muse about what went wrong.
    fn invoke(&mut self, command: &str, params: &str) -> Result<String, String>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Refused {
    Empty,
    TooBig,
    /// Not [`State::Connected`].
    Offline,
    /// The last question has not had its `Done` or `Failed` yet.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Poll {
    /// There is work in hand. Call again soon.
    Active,
    /// Waiting to try again. Nothing happens for this many milliseconds, so
    /// the caller may sleep that long.
    Resting(u32),
    /// The device is unpaired. Further calls do nothing.
    Unpaired,
}

enum Phase<W: crate::Wire> {
    /// The start of a round: save or rotate the tokens if that is due.
    Start,
    Fetch,
    Open { vm: api::Vm, knocks_left: u32 },
    Serving(Session<W>),
    /// Waiting, and then `knock` again if set, else a new round.
    Rest { until_ms: u64, knock: Option<(api::Vm, u32)> },
    Unpaired,
}

enum Refresh {
    /// Use the tokens held now.
    Proceed,
    /// The refresh failed for now. The tokens held are unchanged.
    Failed,
    /// New tokens were issued and could not be saved.
    Unsaved,
    Revoked,
}

pub struct Client<N: Net, C: Clock, V: Vault> {
    net: N,
    clock: C,
    vault: V,
    security: Security,
    api: Endpoint,
    noise: Endpoint,
    node_id: String,
    user_agent: String,
    sdk_token: Option<String>,
    register: Value,
    pairing: Pairing,
    /// Rotated tokens the vault has not accepted yet. The saved ones are
    /// dead, so nothing else happens until these are saved.
    unsaved: Option<Pairing>,
    refresh_tried_ms: Option<u64>,
    /// The SDK token reaches Muse only in a refresh, so each start with one
    /// refreshes once to report it.
    sdk_token_reported: bool,
    /// The API rejected the access token on the round before this one.
    rejected: bool,
    state: State,
    phase: Phase<N::Wire>,
    backoff: Backoff,
    door: Door,
    hello: Option<Hello>,
    ask: Option<Ask>,
}

impl<N: Net, C: Clock, V: Vault> Client<N, C, V> {
    pub fn new(config: Config, pairing: Pairing, security: Security, net: N, clock: C, vault: V) -> Result<Self, BadConfig> {
        if [&config.node_id, &config.display_name, &config.version, &config.user_agent].iter().any(|text| text.is_empty()) {
            return Err(BadConfig::Missing);
        }
        let api_root = config.api_root.as_deref().unwrap_or(DEFAULT_API_ROOT).trim_end_matches('/');
        let noise_host = config.noise_host.as_deref().unwrap_or(DEFAULT_NOISE_HOST);
        let noise_url = if noise_host.contains("://") { noise_host.into() } else { format!("wss://{noise_host}") };
        let reachable = |url: &str| Endpoint::parse(url).filter(|to| security.fits(to)).ok_or(BadConfig::Address);
        let (api, noise) = (reachable(api_root)?, reachable(&noise_url)?);

        let commands: Value = serde_json::from_str(config.commands_json.as_deref().unwrap_or("{}")).map_err(|_| BadConfig::Commands)?;
        if !commands.is_object() {
            return Err(BadConfig::Commands);
        }
        // The VM knows these values from the Linux Device SDK. Family "link"
        // must never be used: the server pushes ESP32 firmware updates to
        // every link device.
        let mut register = json!({
            "node_id": config.node_id,
            "display_name": config.display_name,
            "platform": "linux",
            "version": config.version,
            "device_family": "homehub",
            "model_id": "linux",
            "is_wakeup_supported": false,
            "commands_v2": commands,
        });
        if let Some(ssid) = config.network_ssid.filter(|ssid| !ssid.is_empty()) {
            register["metadata"] = json!({ "network_ssid": ssid });
        }
        Ok(Client {
            net,
            clock,
            vault,
            security,
            api,
            noise,
            node_id: config.node_id,
            user_agent: config.user_agent,
            sdk_token: config.sdk_token.filter(|token| !token.is_empty()),
            register,
            pairing,
            unsaved: None,
            refresh_tried_ms: None,
            sdk_token_reported: false,
            rejected: false,
            state: State::Connecting,
            phase: Phase::Start,
            backoff: Backoff::default(),
            door: Door::default(),
            hello: None,
            ask: None,
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Whether a question is waiting for its `Done` or `Failed`.
    pub fn asking(&self) -> bool {
        self.ask.is_some() || matches!(&self.phase, Phase::Serving(session) if session.asking())
    }

    /// Asks Muse a question. The reply arrives as [`Event`]s during later
    /// polls, ending with exactly one `Done` or `Failed`.
    pub fn ask_text(&mut self, text: &str) -> Result<(), Refused> {
        if text.trim().is_empty() {
            return Err(Refused::Empty);
        }
        if text.len() > LONGEST_QUESTION {
            return Err(Refused::TooBig);
        }
        self.queue(Ask::Text(text.into()))
    }

    /// Asks with a recording, a WAV file of at most 600 KB.
    pub fn ask_voice(&mut self, wav: Vec<u8>) -> Result<(), Refused> {
        if wav.is_empty() {
            return Err(Refused::Empty);
        }
        if wav.len() > LARGEST_WAV {
            return Err(Refused::TooBig);
        }
        self.queue(Ask::Voice(wav))
    }

    fn queue(&mut self, ask: Ask) -> Result<(), Refused> {
        if self.state != State::Connected {
            return Err(Refused::Offline);
        }
        if self.asking() {
            return Err(Refused::Busy);
        }
        self.ask = Some(ask);
        Ok(())
    }

    /// Does the next piece of work. While connected that is reading for up
    /// to `wait_ms`, so a small value keeps the caller's loop lively. While
    /// connecting it is one whole step, such as a TLS handshake and an API
    /// call, which takes as long as the network does: seconds on a PSP.
    pub fn poll(&mut self, app: &mut impl App, wait_ms: u32) -> Poll {
        let now = self.clock.millis();
        let (phase, poll) = match core::mem::replace(&mut self.phase, Phase::Unpaired) {
            Phase::Unpaired => (Phase::Unpaired, Poll::Unpaired),
            Phase::Rest { until_ms, knock } if now < until_ms => {
                (Phase::Rest { until_ms, knock }, Poll::Resting((until_ms - now).min(u64::from(u32::MAX)) as u32))
            }
            Phase::Rest { knock: Some((vm, knocks_left)), .. } => (Phase::Open { vm, knocks_left }, Poll::Active),
            Phase::Rest { knock: None, .. } => (Phase::Start, Poll::Active),
            Phase::Start => self.start(app, now),
            Phase::Fetch => self.fetch(app, now),
            Phase::Open { vm, knocks_left } => self.open(app, now, vm, knocks_left),
            Phase::Serving(mut session) => {
                let stepped = session.step(&self.clock, app, &mut self.ask, wait_ms);
                if session.registered_at_ms.is_some() {
                    self.set_state(app, State::Connected);
                }
                match stepped {
                    Ok(()) => (Phase::Serving(session), Poll::Active),
                    Err(end) => self.ended(app, session, end),
                }
            }
        };
        self.phase = phase;
        poll
    }

    fn set_state(&mut self, app: &mut impl App, state: State) {
        if self.state != state {
            self.state = state;
            app.event(Event::State(state));
        }
    }

    /// A question accepted just before the session went away never reached it.
    fn fail_queued_ask(&mut self, app: &mut impl App) {
        if self.ask.take().is_some() {
            app.event(Event::Failed { text: "", why: "not connected" });
        }
    }

    fn rest(&mut self, app: &mut impl App, wait_ms: u64) -> (Phase<N::Wire>, Poll) {
        self.fail_queued_ask(app);
        log!(app, "trying again in {} s", wait_ms.div_ceil(1000));
        (Phase::Rest { until_ms: self.clock.millis() + wait_ms, knock: None }, Poll::Active)
    }

    /// The round failed before a session was registered.
    fn failed(&mut self, app: &mut impl App, trouble: Trouble) -> (Phase<N::Wire>, Poll) {
        self.set_state(app, State::Unreachable(trouble));
        let wait_ms = self.backoff.next_ms();
        self.rest(app, wait_ms)
    }

    fn unpaired(&mut self, app: &mut impl App) -> (Phase<N::Wire>, Poll) {
        self.set_state(app, State::Unpaired);
        self.fail_queued_ask(app);
        (Phase::Unpaired, Poll::Unpaired)
    }

    fn start(&mut self, app: &mut impl App, now: u64) -> (Phase<N::Wire>, Poll) {
        if let Some(rotated) = self.unsaved.take() {
            if self.vault.save(&rotated).is_err() {
                self.unsaved = Some(rotated);
                log!(app, "could not save the rotated tokens");
                return self.failed(app, Trouble::Storage);
            }
            self.pairing = rotated;
            log!(app, "rotated tokens saved");
        }
        match self.refresh(app, now, false) {
            Refresh::Proceed | Refresh::Failed => (Phase::Fetch, Poll::Active),
            Refresh::Unsaved => self.failed(app, Trouble::Storage),
            Refresh::Revoked => self.unpaired(app),
        }
    }

    /// Rotates the tokens if they are due. `force` is for when the API has
    /// just rejected the access token.
    fn refresh(&mut self, app: &mut impl App, now: u64, force: bool) -> Refresh {
        let report_due = self.sdk_token.is_some() && !self.sdk_token_reported;
        // A clock that was never set, or an age below zero, says nothing
        // about the token.
        let age = match (self.clock.unix_seconds(), self.pairing.saved_at) {
            (Some(unix), saved_at) if saved_at > 0 => unix.checked_sub(saved_at),
            _ => None,
        };
        let due = force || age.is_none_or(|age| age >= REFRESH_AGE_S);
        if !due && !report_due {
            return Refresh::Proceed;
        }
        if !force && self.refresh_tried_ms.is_some_and(|tried| now.saturating_sub(tried) < REFRESH_RETRY_MS) {
            return Refresh::Proceed;
        }
        self.refresh_tried_ms = Some(now);
        if report_due {
            self.sdk_token_reported = true;
            log!(app, "refreshing the device token to report the SDK token");
        }
        let rotated = api::refresh(
            &mut self.net,
            &self.clock,
            &self.security,
            &self.api,
            &self.pairing.refresh_token,
            &self.node_id,
            self.sdk_token.as_deref(),
            &self.user_agent,
        );
        match rotated {
            Ok((access_token, refresh_token)) => {
                let rotated = Pairing { access_token, refresh_token, saved_at: self.clock.unix_seconds().unwrap_or(0) };
                if self.vault.save(&rotated).is_err() {
                    self.unsaved = Some(rotated);
                    log!(app, "could not save the rotated tokens");
                    return Refresh::Unsaved;
                }
                self.pairing = rotated;
                log!(app, "device token rotated");
                Refresh::Proceed
            }
            // Only reporting the SDK token: nothing has rejected the current
            // token, so a refusal here must never unpair the device.
            Err(fault) if !due => {
                log!(app, "the SDK token report failed ({fault:?}), keeping the pairing");
                Refresh::Proceed
            }
            Err(Fault::Refused(401)) => {
                log!(app, "pairing revoked, the device has to be set up again in the Muse app");
                Refresh::Revoked
            }
            Err(fault) => {
                log!(app, "token refresh failed: {fault:?}");
                Refresh::Failed
            }
        }
    }

    fn fetch(&mut self, app: &mut impl App, now: u64) -> (Phase<N::Wire>, Poll) {
        let fetched = api::fetch_vm(&mut self.net, &self.clock, &self.security, &self.api, &self.pairing.access_token, &self.user_agent);
        let rejected = core::mem::replace(&mut self.rejected, matches!(fetched, Err(Fault::Refused(401))));
        match fetched {
            Err(Fault::Refused(401)) => {
                log!(app, "device token rejected by the API, refreshing");
                match self.refresh(app, now, true) {
                    Refresh::Revoked => self.unpaired(app),
                    Refresh::Unsaved => self.failed(app, Trouble::Storage),
                    // The token held is known bad, so there is no hurry.
                    Refresh::Failed => {
                        self.set_state(app, State::Unreachable(Trouble::Refused(401)));
                        self.rest(app, REFRESH_RETRY_MS)
                    }
                    // The new token is tried at once the first time. A server
                    // that rejects every token it hands out must not have
                    // the device rotate them in a tight loop.
                    Refresh::Proceed if rejected => self.failed(app, Trouble::Refused(401)),
                    Refresh::Proceed => (Phase::Fetch, Poll::Active),
                }
            }
            Err(fault) => {
                log!(app, "VM fetch failed: {fault:?}");
                self.failed(app, fault.into())
            }
            Ok(None) => {
                log!(app, "the API leased no VM to this device");
                self.failed(app, Trouble::NoVm)
            }
            Ok(Some(vm)) => {
                let knocks_left = self.door.allowance(vm.knock, now);
                (Phase::Open { vm, knocks_left }, Poll::Active)
            }
        }
    }

    fn open(&mut self, app: &mut impl App, now: u64, vm: api::Vm, knocks_left: u32) -> (Phase<N::Wire>, Poll) {
        let mut path = String::from("/v1/noise?vm_id=");
        encode_component(&vm.target, &mut path);
        log!(app, "connecting to {}", vm.name);
        let opened = Session::open(
            &mut self.net,
            &self.clock,
            &self.security,
            &self.noise.at(&path),
            &vm.bearer,
            &self.node_id,
            &self.register,
            &mut self.hello,
        );
        match opened {
            Ok(session) => (Phase::Serving(session), Poll::Active),
            // A VM nothing has reached for a while is not behind its front
            // door yet, and the door says 403 until it is. Asked again every
            // half second it opens within seconds. Asked once a minute it
            // never does, because whatever brings the VM there has let go
            // again by then. The API says how often to ask and how many
            // times.
            Err(Fault::Refused(403)) if knocks_left > 0 => {
                self.door.knocked(now);
                let until_ms = self.clock.millis() + u64::from(vm.knock.every_ms);
                (Phase::Rest { until_ms, knock: Some((vm, knocks_left - 1)) }, Poll::Active)
            }
            Err(fault) => {
                log!(app, "the VM did not open a session: {fault:?}");
                if let Fault::Refused(401 | 403) = fault {
                    // Either way the next round fetches a new bearer.
                    self.backoff.at_least(REFUSED_WAIT_MS);
                }
                self.failed(app, fault.into())
            }
        }
    }

    fn ended(&mut self, app: &mut impl App, session: Session<N::Wire>, end: End) -> (Phase<N::Wire>, Poll) {
        let lasted_ms = session.registered_at_ms.map(|at| self.clock.millis().saturating_sub(at));
        session.close(app);
        log!(app, "session ended: {end:?} after {} s registered", lasted_ms.unwrap_or(0) / 1000);
        let fault = match end {
            End::Unpaired => return self.unpaired(app),
            End::Failed(fault) => fault,
        };
        if lasted_ms.is_some_and(|lasted| lasted >= HEALTHY_SESSION_MS) {
            self.backoff.reset();
        }
        if let Fault::Refused(401 | 403) = fault {
            self.backoff.at_least(REFUSED_WAIT_MS);
        }
        if lasted_ms.is_none() {
            return self.failed(app, fault.into());
        }
        // It was working a moment ago, so this is connecting again, not
        // unreachable, until a try says otherwise.
        self.set_state(app, State::Connecting);
        let wait_ms = self.backoff.next_ms();
        self.rest(app, wait_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{secure, Lost, Stream, Wire};
    use alloc::sync::Arc;
    use rustls::pki_types::UnixTime;
    use rustls::time_provider::TimeProvider;

    #[derive(Debug)]
    struct NoDate;
    impl TimeProvider for NoDate {
        fn current_time(&self) -> Option<UnixTime> {
            None
        }
    }

    struct Nowhere;
    impl Stream for Nowhere {
        fn read(&mut self, _: &mut [u8]) -> Result<usize, Lost> {
            Err(Lost)
        }
        fn write_all(&mut self, _: &[u8]) -> Result<(), Lost> {
            Err(Lost)
        }
    }
    impl Wire for Nowhere {
        fn readable(&mut self, _: u32) -> Result<bool, Lost> {
            Err(Lost)
        }
    }
    impl Net for Nowhere {
        type Wire = Nowhere;
        fn open(&mut self, _: &str, _: u16) -> Result<Nowhere, Lost> {
            Err(Lost)
        }
    }
    impl Clock for Nowhere {
        fn millis(&self) -> u64 {
            0
        }
        fn unix_seconds(&self) -> Option<u64> {
            None
        }
    }
    impl Vault for Nowhere {
        fn save(&mut self, _: &Pairing) -> Result<(), NotSaved> {
            Err(NotSaved)
        }
    }
    impl App for Vec<String> {
        fn event(&mut self, event: Event<'_>) {
            match event {
                Event::State(state) => self.push(format!("{state:?}")),
                Event::Failed { why, .. } => self.push(format!("failed: {why}")),
                _ => {}
            }
        }
        fn invoke(&mut self, _: &str, _: &str) -> Result<String, String> {
            Err("no commands".into())
        }
    }

    fn client(change: impl FnOnce(&mut Config)) -> Result<Client<Nowhere, Nowhere, Nowhere>, BadConfig> {
        let mut config = Config::new("homelink-1", "PSP", "0.1.0", "muse-psp");
        change(&mut config);
        let pairing = Pairing { access_token: "a".into(), refresh_token: "r".into(), saved_at: 0 };
        Client::new(config, pairing, Security::Tls(secure::settings(Arc::new(NoDate)).unwrap()), Nowhere, Nowhere, Nowhere)
    }

    #[test]
    fn tls_takes_no_plaintext_address() {
        assert!(client(|_| {}).is_ok(), "the real addresses");
        assert!(client(|config| config.noise_host = Some("wss://hatch.example:8443".into())).is_ok());
        for api_root in ["http://api.muse.ai", "ws://api.muse.ai", "api.muse.ai", ""] {
            assert_eq!(client(|config| config.api_root = Some(api_root.into())).err(), Some(BadConfig::Address), "{api_root:?}");
        }
        for noise_host in ["ws://hatch.metaaivm.com", "http://hatch.metaaivm.com", "wss://"] {
            assert_eq!(client(|config| config.noise_host = Some(noise_host.into())).err(), Some(BadConfig::Address), "{noise_host:?}");
        }
    }

    #[test]
    fn rejects_a_config_it_could_not_register_with() {
        assert_eq!(client(|config| config.commands_json = Some("[]".into())).err(), Some(BadConfig::Commands));
        assert_eq!(client(|config| config.commands_json = Some("{".into())).err(), Some(BadConfig::Commands));
        assert_eq!(client(|config| config.node_id.clear()).err(), Some(BadConfig::Missing));
    }

    #[test]
    fn registers_as_a_linux_home_hub() {
        let client = client(|config| {
            config.commands_json = Some(r#"{"psp.say":{}}"#.into());
            config.network_ssid = Some("Home".into());
        })
        .unwrap();
        assert_eq!(
            client.register,
            json!({
                "node_id": "homelink-1", "display_name": "PSP", "platform": "linux", "version": "0.1.0",
                "device_family": "homehub", "model_id": "linux", "is_wakeup_supported": false,
                "commands_v2": {"psp.say": {}}, "metadata": {"network_ssid": "Home"},
            })
        );
    }

    #[test]
    fn with_no_network_it_says_unreachable_and_rests_with_growing_waits() {
        let mut client = client(|_| {}).unwrap();
        let mut seen = Vec::new();
        assert_eq!(client.state(), State::Connecting);
        assert_eq!(client.ask_text("too early"), Err(Refused::Offline));
        let mut rests = Vec::new();
        for _ in 0..12 {
            if let Poll::Resting(ms) = client.poll(&mut seen, 0) {
                rests.push(ms);
                // The clock here never moves, so the wait is skipped by hand.
                client.phase = Phase::Start;
            }
        }
        assert_eq!(seen, ["Unreachable(Network)"], "said once, and it stays");
        assert_eq!(rests, [2_000, 4_000, 8_000, 16_000]);
        assert_eq!(client.ask_voice(Vec::new()), Err(Refused::Empty));
        assert_eq!(client.ask_voice(alloc::vec![0; LARGEST_WAV + 1]), Err(Refused::TooBig));
    }
}
