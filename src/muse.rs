//! The connection to Muse, on a thread of its own. Reaching Muse takes
//! seconds at a time on a PSP, and the avatar must keep moving meanwhile.

use crate::{clock, entropy, net, store, voice};
use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU8, Ordering};
use muse_link::{secure, App, Client, Clock, Config, Event, Lost, Net, Poll, Security, State};
use psp::sys::{self, ThreadAttributes};

/// Which saved connection in the PSP's Network Settings to join. The first,
/// unless L was held as the app started, which picks the second. That is
/// for trying another network without editing the first.
pub static WIFI_PROFILE: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(1);
/// Below the screen loop's 32, where a larger number yields. The PSP does not
/// share time between equals, so at 32 a handshake would freeze the avatar.
const PRIORITY: i32 = 40;

pub const CONNECTING: u8 = 0;
pub const CONNECTED: u8 = 1;
/// The last try failed and another is coming.
pub const TROUBLE: u8 = 2;
/// Nothing more will be tried: there is no pairing, or Muse removed it.
pub const DOWN: u8 = 3;
pub static LINK: AtomicU8 = AtomicU8::new(CONNECTING);
/// When the trouble began, on `report::now_ms`'s clock. One slow start is
/// not worth alarming anyone over, so the screen waits before calling it.
pub static TROUBLE_SINCE_MS: AtomicU32 = AtomicU32::new(0);

fn trouble() {
    if LINK.swap(TROUBLE, Ordering::Relaxed) != TROUBLE {
        TROUBLE_SINCE_MS.store(crate::report::now_ms(), Ordering::Relaxed);
    }
}

/// When the dance Muse asked for ends, on `report::now_ms`'s clock.
pub static DANCE_UNTIL_MS: AtomicU32 = AtomicU32::new(0);

/// What Muse can ask this PSP to do, in the form it calls commands_v2.
const COMMANDS: &str = r#"{
 "psp.say": {
  "description": "Speak a short message aloud through the speaker of the user's Sony PSP. Keep it under about 200 characters.",
  "required": {"text": {"type": "string", "description": "The message to speak."}},
  "optional": {}
 },
 "psp.dance": {
  "description": "Make the Muse avatar on the user's PSP dance. The avatar is you, so call this when the user says 'make Muse dance', 'dance', or asks you or their PSP to dance.",
  "required": {},
  "optional": {"seconds": {"type": "number", "description": "How long, 2 to 60. Default 8."}}
 },
 "psp.status": {
  "description": "Read the user's Sony PSP's battery level, whether it is charging or plugged in, and its Wi-Fi network and signal strength. Call this when the user asks about their PSP's battery, power or connection.",
  "required": {},
  "optional": {}
 }
}"#;

pub const NO_QUESTION: u8 = 0;
pub const ASKED: u8 = 1;
pub const ANSWERED: u8 = 2;
pub static TURN: AtomicU8 = AtomicU8::new(NO_QUESTION);

/// A recording on its way from the audio thread. A pointer swap needs no
/// lock, and a lock between threads of different rank can hang here.
static TAKE: AtomicPtr<Vec<u8>> = AtomicPtr::new(core::ptr::null_mut());

/// Hands a WAV recording to Muse as the next question.
pub fn ask(wav: Vec<u8>) {
    TURN.store(ASKED, Ordering::Relaxed);
    let waiting = TAKE.swap(Box::into_raw(Box::new(wav)), Ordering::AcqRel);
    if !waiting.is_null() {
        drop(unsafe { Box::from_raw(waiting) });
    }
}

fn taken() -> Option<Vec<u8>> {
    let take = TAKE.swap(core::ptr::null_mut(), Ordering::AcqRel);
    (!take.is_null()).then(|| *unsafe { Box::from_raw(take) })
}

pub struct Psp;

impl Net for Psp {
    type Wire = net::Socket;

    fn open(&mut self, host: &str, port: u16) -> Result<net::Socket, Lost> {
        net::lookup(host).and_then(|address| net::Socket::connect(address, port)).map_err(|why| {
            crate::say!("net: {}", why);
            Lost
        })
    }
}

impl Clock for Psp {
    fn millis(&self) -> u64 {
        unsafe { sys::sceKernelGetSystemTimeWide() as u64 / 1000 }
    }

    fn unix_seconds(&self) -> Option<u64> {
        clock::unix_seconds()
    }
}

/// What the client tells the app, turned into what the avatar and the
/// voice do.
#[derive(Default)]
struct Avatar {
    /// Whether this question's reply has been sent to be spoken.
    spoken: bool,
    /// The recording, until Muse says it heard it. A PSP's upload stalls
    /// often enough that one lost on the way is asked again without the
    /// user having to.
    unheard: Option<Vec<u8>>,
    again: Again,
}

#[derive(Default, PartialEq)]
enum Again {
    #[default]
    Allowed,
    /// To be asked when Muse is next reachable, unless that takes too long.
    Wanted { since_ms: u32 },
    Used,
}

/// How long a question waits for Muse to come back before it is dropped.
const AGAIN_WITHIN_MS: u32 = 60_000;

impl Avatar {
    fn speak(&mut self, text: &str) {
        if !core::mem::replace(&mut self.spoken, true) && !text.trim().is_empty() {
            voice::say(text);
        }
    }
}

impl App for Avatar {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::State(state) => {
                crate::say!("muse: {:?}", state);
                match state {
                    State::Connected => LINK.store(CONNECTED, Ordering::Relaxed),
                    // A drop is followed by a new try at once, which says
                    // nothing yet about whether Muse can be reached.
                    State::Connecting if LINK.load(Ordering::Relaxed) != TROUBLE => LINK.store(CONNECTING, Ordering::Relaxed),
                    State::Connecting => {}
                    State::Unreachable(_) => trouble(),
                    State::Unpaired => LINK.store(DOWN, Ordering::Relaxed),
                }
            }
            Event::Log(line) => crate::say!("muse: {}", line),
            Event::Heard(text) => {
                crate::say!("heard: {}", text);
                self.unheard = None;
            }
            Event::Reply(_) => {}
            // The stream has paused, which is almost always the whole reply,
            // a couple of seconds before `Done` confirms it.
            Event::Settled(text) => {
                crate::say!("settled: {} characters", text.len());
                self.speak(text);
            }
            Event::Done(text) => {
                crate::say!("reply: {}", text);
                self.speak(text);
                self.spoken = false;
                self.unheard = None;
                TURN.store(ANSWERED, Ordering::Relaxed);
            }
            Event::Failed { why, .. } => {
                self.spoken = false;
                if let Some((_, signal)) = net::wifi() {
                    crate::say!("signal {} of 100 when the question failed", signal);
                }
                if self.unheard.is_some() && self.again == Again::Allowed {
                    crate::say!("question failed: {}, asking again", why);
                    self.again = Again::Wanted { since_ms: crate::report::now_ms() };
                } else {
                    crate::say!("question failed: {}", why);
                    self.unheard = None;
                    TURN.store(NO_QUESTION, Ordering::Relaxed);
                }
            }
        }
    }

    fn invoke(&mut self, command: &str, params: &str) -> Result<String, String> {
        crate::say!("command: {}", command);
        let params: serde_json::Value = serde_json::from_str(params).map_err(|_| String::from("the parameters are not JSON"))?;
        match command {
            "psp.say" => {
                let text = params["text"].as_str().map(str::trim).filter(|text| !text.is_empty()).ok_or("there is no text to say")?;
                if voice::STATE.load(Ordering::Relaxed) != voice::SILENT {
                    return Err("the PSP is already speaking".into());
                }
                voice::say(text);
                Ok("{}".into())
            }
            "psp.dance" => {
                let seconds = params["seconds"].as_f64().unwrap_or(8.0).clamp(2.0, 60.0) as u32;
                DANCE_UNTIL_MS.store(crate::report::now_ms().wrapping_add(seconds * 1000), Ordering::Relaxed);
                Ok("{}".into())
            }
            "psp.status" => {
                let (percent, charging, plugged) = unsafe {
                    (sys::scePowerGetBatteryLifePercent(), sys::scePowerIsBatteryCharging() > 0, sys::scePowerIsPowerOnline() > 0)
                };
                let wifi = net::wifi();
                Ok(serde_json::json!({
                    "battery_percent": percent,
                    "charging": charging,
                    "plugged_in": plugged,
                    "wifi_network": wifi.as_ref().map(|(name, _)| name.as_str()),
                    "wifi_signal_percent": wifi.as_ref().map(|&(_, strength)| strength),
                })
                .to_string())
            }
            _ => Err(format!("this PSP does not run {}", command)),
        }
    }
}

fn serve() -> Result<(), String> {
    let profile = WIFI_PROFILE.load(Ordering::Relaxed);
    while let Err(why) = net::join(profile) {
        crate::say!("{}, trying again in 5 s", why);
        trouble();
        unsafe { sys::sceKernelDelayThread(5_000_000) };
    }
    match net::wifi() {
        Some((name, signal)) => crate::say!("Wi-Fi joined: connection {}, {:?}, signal {} of 100", profile, name, signal),
        None => crate::say!("Wi-Fi joined: connection {}", profile),
    }
    let saved = store::load()?;
    let mut config = Config::new(&saved.node_id, "PSP", env!("CARGO_PKG_VERSION"), concat!("muse-gadget-psp/", env!("CARGO_PKG_VERSION")));
    config.commands_json = Some(COMMANDS.into());
    config.sdk_token = saved.sdk_token;
    config.api_root = saved.api_root;
    config.noise_host = saved.noise_host;
    let security = Security::Tls(secure::settings(Arc::new(clock::Rtc)).map_err(|error| format!("{:?}", error))?);
    let mut client = Client::new(config, saved.pairing, security, Psp, Psp, saved.store).map_err(|error| format!("{:?}", error))?;
    let mut app = Avatar::default();
    loop {
        match client.poll(&mut app, 10) {
            Poll::Active => {}
            Poll::Resting(wait_ms) => unsafe {
                sys::sceKernelDelayThread(wait_ms.min(50) * 1000);
            },
            Poll::Unpaired => return Err("Muse no longer knows this PSP. It has to be paired again.".into()),
        }
        let fresh = taken();
        if let Some(wav) = &fresh {
            entropy::stir(&wav[wav.len() / 2..]);
            app.again = Again::Allowed;
        }
        let again = match app.again {
            Again::Wanted { since_ms } if crate::report::now_ms().wrapping_sub(since_ms) > AGAIN_WITHIN_MS => {
                crate::say!("question dropped: Muse did not come back in time");
                app.again = Again::Used;
                app.unheard = None;
                TURN.store(NO_QUESTION, Ordering::Relaxed);
                None
            }
            Again::Wanted { .. } if client.state() == State::Connected && !client.asking() => {
                app.again = Again::Used;
                app.unheard.take()
            }
            _ => None,
        };
        if let Some(wav) = fresh.or(again) {
            app.unheard = Some(wav.clone());
            if let Err(refused) = client.ask_voice(wav) {
                crate::say!("question refused: {:?}", refused);
                app.unheard = None;
                TURN.store(NO_QUESTION, Ordering::Relaxed);
            }
        }
    }
}

unsafe extern "C" fn run(_: usize, _: *mut c_void) -> i32 {
    if let Err(why) = serve() {
        crate::say!("FAIL: {}", why);
    }
    LINK.store(DOWN, Ordering::Relaxed);
    0
}

/// Call after `net::start`, so the network modules load before this thread's stack exists.
pub fn start() {
    unsafe {
        let thread = sys::sceKernelCreateThread(
            b"muse\0".as_ptr(),
            run,
            PRIORITY,
            256 * 1024,
            ThreadAttributes::USER | ThreadAttributes::VFPU,
            core::ptr::null_mut(),
        );
        sys::sceKernelStartThread(thread, 0, core::ptr::null_mut());
    }
}
