#![no_std]
#![no_main]

extern crate alloc;

mod audio;
mod avatar;
mod call;
mod clock;
mod entropy;
mod files;
mod muse;
mod net;
mod report;
mod screen;
mod startup;
mod status;
mod store;
mod voice;

use avatar::{Avatar, Mood};
use call::Line;
use core::sync::atomic::Ordering;
use psp::sys::{self, CtrlButtons, SceCtrlData};
use report::Timer;
use screen::Screen;
use status::Link;

/// How long Muse can be out of reach before the screen says so. A Muse that
/// has been idle can take a while to wake, and the app keeps trying.
const PATIENCE_MS: u32 = 45_000;
/// How long the avatar shows that an answer came when it cannot be spoken.
const UNSPOKEN_MS: u32 = 3000;

fn pressed() -> CtrlButtons {
    let mut pad: SceCtrlData = unsafe { core::mem::zeroed() };
    unsafe { sys::sceCtrlPeekBufferPositive(&mut pad, 1) };
    pad.buttons
}

fn psp_main() {
    psp::enable_home_button();
    unsafe { sys::scePowerSetClockFrequency(333, 333, 166) };
    report::begin();
    say!("Muse {} for the PSP, in {}", env!("CARGO_PKG_VERSION"), core::str::from_utf8(files::home()).unwrap_or("?"));
    entropy::seed();
    let Some(avatar) = files::read("avatar.bin").and_then(Avatar::parse) else {
        say!("FAIL: avatar.bin is missing or not one tools/avatar.sh wrote");
        return;
    };
    // The network modules load before the connection threads' stacks exist.
    // Loaded after, the second one failed for lack of room.
    if pressed().contains(CtrlButtons::LTRIGGER) {
        muse::WIFI_PROFILE.store(2, Ordering::Relaxed);
    }
    match net::start() {
        Ok(()) => {
            muse::start();
            voice::start();
            call::start();
        }
        Err(why) => {
            say!("FAIL: {}", why);
            muse::LINK.store(muse::DOWN, Ordering::Relaxed);
        }
    }
    audio::start();

    let mut call_mode = call::load_mode();
    say!("mode: {}", if call_mode { "calls" } else { "voice notes" });

    let mut screen = Screen::open();
    let mut mood = Mood::Boot;
    let mut since = Timer::start();
    let mut shown_ms = 0;
    let mut held = CtrlButtons::empty();
    loop {
        let now_ms = report::now_ms();
        let link = match muse::LINK.load(Ordering::Relaxed) {
            muse::CONNECTED => Link::Online,
            muse::DOWN => Link::Offline,
            muse::TROUBLE if now_ms.wrapping_sub(muse::TROUBLE_SINCE_MS.load(Ordering::Relaxed)) > PATIENCE_MS => Link::Offline,
            _ => Link::Connecting,
        };
        let mut turn = muse::TURN.load(Ordering::Relaxed);
        let pad = pressed();
        let just_pressed = pad.difference(held);
        held = pad;
        // In call mode R talks on a call, so no voice note can start.
        let may_talk = !call_mode && link == Link::Online && turn == muse::NO_QUESTION;
        audio::TALKING.store(may_talk && pad.contains(CtrlButtons::RTRIGGER), Ordering::Relaxed);
        let speech = voice::STATE.load(Ordering::Relaxed);
        // Voice-note mode shows nothing of calls, a call that just ended included.
        let line = if call_mode { call::line(now_ms) } else { Line::Idle };
        let talk = matches!(line, Line::Live { .. }) && pad.contains(CtrlButtons::RTRIGGER);
        call::TALK.store(talk, Ordering::Relaxed);
        if call_mode && just_pressed.contains(CtrlButtons::START) {
            // A reply that could not be spoken stays FAILED until a voice
            // note's turn ends, so only a voice that is busy holds off a call.
            let voice_busy = matches!(speech, voice::PREPARING | voice::SPEAKING);
            match line {
                Line::Dialing | Line::Live { .. } => call::hang_up(),
                _ if line.idle() && link == Link::Online && !voice_busy => call::dial(),
                _ => {}
            }
        }
        if just_pressed.contains(CtrlButtons::SELECT) && line.idle() && turn == muse::NO_QUESTION && !audio::RECORDING.load(Ordering::Relaxed) {
            call_mode = !call_mode;
            call::keep_mode(call_mode);
            say!("mode: {}", if call_mode { "calls" } else { "voice notes" });
            if !call_mode {
                // Nothing draws the call's row in voice-note mode. Cleared
                // here, before the avatar, which then redraws its own part.
                status::clear_call(&mut screen);
            }
        }
        if turn == muse::ANSWERED {
            // The turn ends when the reply has been spoken. If it could not
            // be, the avatar celebrates the answer in silence for a moment.
            let unspoken_shown = speech == voice::FAILED && mood == Mood::Happy && shown_ms >= UNSPOKEN_MS;
            if unspoken_shown {
                voice::STATE.store(voice::SILENT, Ordering::Relaxed);
            }
            if unspoken_shown || speech == voice::SILENT {
                turn = muse::NO_QUESTION;
                muse::TURN.store(turn, Ordering::Relaxed);
            }
        }
        let dancing = (muse::DANCE_UNTIL_MS.load(Ordering::Relaxed).wrapping_sub(now_ms) as i32) > 0;
        let sounding = call_mode && call::sounding(now_ms);
        let next = match (audio::RECORDING.load(Ordering::Relaxed), speech, turn, link) {
            _ if talk => Mood::Listening,
            _ if sounding => Mood::Speaking,
            _ if matches!(line, Line::Dialing | Line::Ending | Line::Live { working: true, .. }) => Mood::Thinking,
            _ if matches!(line, Line::Ended { failed: true, .. }) => Mood::Error,
            _ if matches!(line, Line::Live { .. }) => Mood::Idle,
            (true, _, _, _) => Mood::Listening,
            (_, voice::SPEAKING, _, _) => Mood::Speaking,
            (_, voice::FAILED, muse::ANSWERED, _) => Mood::Happy,
            (_, _, muse::ASKED | muse::ANSWERED, _) => Mood::Thinking,
            (_, _, _, Link::Online) if dancing => Mood::Happy,
            (_, _, _, Link::Connecting) => Mood::Boot,
            (_, _, _, Link::Online) => Mood::Idle,
            (_, _, _, Link::Offline) => Mood::Error,
        };
        if next != mood {
            say!("mood {:?}", next);
            mood = next;
            shown_ms = 0;
            since.lap();
        }
        shown_ms += since.lap();
        avatar.draw(&mut screen, mood, shown_ms);
        status::draw(&mut screen, link, now_ms);
        if call_mode {
            status::draw_call(&mut screen, line, now_ms);
        }
        screen.next_frame();
    }
}
