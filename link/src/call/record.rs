//! The records of a call, one JSON object per line. Those going up are built
//! as bytes, ready to send. Each line coming down is parsed into a `Down`,
//! the only shape the rest of the call sees.

use super::CALL_RATE;
use crate::ws;
use alloc::{format, string::String, string::ToString, vec::Vec};
use core::fmt::Write;
use serde_json::Value;

/// One audio event holds at most 4 s.
const LARGEST_AUDIO: usize = CALL_RATE as usize * 2 * 4;
const LONGEST_ID: usize = 128;

/// An id from the server: 1 to 128 bytes with no NUL. Only `parse` makes
/// one, so every id in a call has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Id(String);

impl Id {
    pub(crate) fn parse(value: Option<&Value>) -> Option<Id> {
        let text = value?.as_str()?;
        (!text.is_empty() && text.len() <= LONGEST_ID && !text.contains('\0')).then(|| Id(text.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
pub(crate) fn id(text: &str) -> Id {
    Id::parse(Some(&Value::from(text))).unwrap()
}

/// What a line from the server means to this client.
#[derive(Debug, PartialEq)]
pub(crate) enum Down {
    /// hatch.voice.session: the call is ready.
    Session { call_id: Id, session_id: Id, thread_id: Option<Id> },
    /// A hatch.voice.session without a usable call id or session id, or with
    /// a thread id that is not one.
    BadSession,
    /// `dead`: the session is gone (code session_dead). The message is not
    /// kept, since it can hold what the user said.
    Error { dead: bool },
    /// hatch.voice.switch_input_hold: no microphone audio while true.
    Hold(bool),
    SpeechStarted(Option<Id>),
    SpeechStopped(Option<Id>),
    TurnStarted { submission: Id, item: Id },
    /// A hatch.voice.turn_started without both of its ids.
    BadTurn,
    TurnSettled(Id),
    InputSuppressed(Option<Id>),
    ResponseCreated(Option<Id>),
    /// response.done or response.completed. Its audio can still be arriving.
    ResponseDone(Option<Id>),
    /// Muse's voice, mono at [`CALL_RATE`]. Only continuous_audio_video.delta
    /// names its response.
    Audio { response_id: Option<Id>, pcm: Vec<i16> },
    /// Audio that does not decode, has an odd length or another format, or
    /// runs over 4 s.
    BadAudio,
    /// Muse said goodbye: a session_control item named stop.
    Stop,
    Other,
}

pub(crate) fn parse(line: &[u8]) -> Down {
    let Ok(event) = serde_json::from_slice::<Value>(line) else { return Down::Other };
    let id = |key: &str| Id::parse(event.get(key));
    match event.get("type").and_then(Value::as_str).unwrap_or_default() {
        "hatch.voice.session" => session(&event),
        "error" => Down::Error { dead: event.pointer("/error/code").and_then(Value::as_str) == Some("session_dead") },
        "hatch.voice.switch_input_hold" => event.get("held").and_then(Value::as_bool).map_or(Down::Other, Down::Hold),
        "input_audio_buffer.speech_started" => Down::SpeechStarted(id("item_id")),
        "input_audio_buffer.speech_stopped" => Down::SpeechStopped(id("item_id")),
        "hatch.voice.turn_started" => match (id("submission_message_id"), id("item_id")) {
            (Some(submission), Some(item)) => Down::TurnStarted { submission, item },
            _ => Down::BadTurn,
        },
        "hatch.voice.turn_settled" => id("response_message_id").map_or(Down::Other, Down::TurnSettled),
        "hatch.voice.input_suppressed" => Down::InputSuppressed(id("item_id")),
        "response.created" => Down::ResponseCreated(Id::parse(event.pointer("/response/id"))),
        "response.done" | "response.completed" => Down::ResponseDone(Id::parse(event.pointer("/response/id"))),
        "continuous_audio_video.delta" => match event.get("audio_delta") {
            None | Some(Value::Null) => Down::Other,
            Some(delta) if !pcm_format(delta) => Down::BadAudio,
            Some(delta) => audio(delta.get("audio_frames"), id("response_id")),
        },
        "response.output_audio.delta" | "response.audio.delta" => audio(event.get("delta"), None),
        "conversation.item.added" if stop(&event) => Down::Stop,
        _ => Down::Other,
    }
}

fn session(event: &Value) -> Down {
    // A session or thread id of only spaces names nothing.
    let named = |key: &str| Id::parse(event.get(key)).filter(|id| !id.0.trim().is_empty());
    let thread_id = match event.get("thread_id") {
        None | Some(Value::Null) => Some(None),
        Some(_) => named("thread_id").map(Some),
    };
    match (Id::parse(event.get("call_id")), named("session_id"), thread_id) {
        (Some(call_id), Some(session_id), Some(thread_id)) => Down::Session { call_id, session_id, thread_id },
        _ => Down::BadSession,
    }
}

/// PCM16 mono at the call's rate, as far as the audio says.
fn pcm_format(delta: &Value) -> bool {
    let number_is = |value: Option<&Value>, want: f64| value.and_then(Value::as_f64) == Some(want);
    let format = match delta.get("format") {
        None | Some(Value::Null) => true,
        Some(format) => {
            matches!(format.get("type").map(Value::as_str), None | Some(Some("" | "audio/pcm")))
                && number_is(format.get("rate"), f64::from(CALL_RATE))
        }
    };
    format && delta.get("num_channels").is_none_or(|channels| number_is(Some(channels), 1.0))
}

fn audio(encoded: Option<&Value>, response_id: Option<Id>) -> Down {
    let Some(encoded) = encoded.and_then(Value::as_str) else { return Down::BadAudio };
    let mut bytes = Vec::new();
    if ws::unbase64(encoded.as_bytes(), LARGEST_AUDIO, &mut bytes).is_none() || bytes.len() % 2 != 0 {
        return Down::BadAudio;
    }
    if bytes.is_empty() {
        return Down::Other;
    }
    let pcm = bytes.chunks_exact(2).map(|pair| i16::from_le_bytes([pair[0], pair[1]])).collect();
    Down::Audio { response_id, pcm }
}

/// Its call_id names the control, never the call, so it is not compared.
fn stop(event: &Value) -> bool {
    let item = |key: &str| event.get("item").and_then(|item| item.get(key)).and_then(Value::as_str);
    item("type") == Some("session_control") && item("name").is_some_and(|name| name.trim().eq_ignore_ascii_case("stop"))
}

fn json_string(text: &str) -> String {
    Value::from(text).to_string()
}

/// Says this client ends the call on Muse's goodbye, so Muse may end it that
/// way.
pub(crate) const CAPABILITY_OFFER: &[u8] = b"{\"type\":\"hatch.voice.capability.offer\",\"end_call_stop\":1}\n";

/// The first record up: the call this media stream is for.
pub(crate) fn admission(call_id: &str, device_id: &str) -> Vec<u8> {
    format!(
        "{{\"type\":\"hatch.voice.call.start\",\"call_id\":{},\"device\":\"mcu-wearable\",\"device_id\":{},\"call_media_type\":\"audio\",\"proactive_greeting_enabled\":false}}\n",
        json_string(call_id),
        json_string(device_id)
    )
    .into_bytes()
}

/// Writes the record for `pcm` over `line`.
pub(crate) fn append(pcm: &[i16], line: &mut Vec<u8>) {
    line.clear();
    line.extend_from_slice(b"{\"type\":\"input_audio_buffer.append\",\"audio\":\"");
    let mut bytes = [0u8; 6];
    // Three samples are six bytes, which base64 takes without padding.
    for samples in pcm.chunks(3) {
        for (pair, sample) in bytes.chunks_exact_mut(2).zip(samples) {
            pair.copy_from_slice(&sample.to_le_bytes());
        }
        ws::base64(&bytes[..2 * samples.len()], line);
    }
    line.extend_from_slice(b"\"}\n");
}

pub(crate) fn cancel(response_id: &Id) -> Vec<u8> {
    format!("{{\"type\":\"response.cancel\",\"response_id\":{}}}\n", json_string(response_id.as_str())).into_bytes()
}

/// The body of the end-call request. The call started at `started_unix_ms`
/// and lasted `lasted_ms`, and it ends at least 1 ms after it starts.
pub(crate) fn end_call_body(call_id: &str, started_unix_ms: u64, lasted_ms: u64, thread_id: Option<&Id>) -> Vec<u8> {
    let ended_unix_ms = started_unix_ms + lasted_ms.max(1);
    let mut body = format!(
        "{{\"call_id\":{},\"call_started_at\":\"{}\",\"call_ended_at\":\"{}\"",
        json_string(call_id),
        rfc3339(started_unix_ms),
        rfc3339(ended_unix_ms)
    );
    if let Some(thread_id) = thread_id {
        let _ = write!(body, ",\"thread_id\":{}", json_string(thread_id.as_str()));
    }
    body.push('}');
    body.into_bytes()
}

/// "YYYY-MM-DDTHH:MM:SS.mmmZ", from days to a date as Howard Hinnant's
/// civil_from_days does it.
pub(crate) fn rfc3339(unix_ms: u64) -> String {
    let (days, ms) = (unix_ms / 86_400_000, unix_ms % 86_400_000);
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    let seconds = ms / 1000;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", seconds / 3600, seconds / 60 % 60, seconds % 60, ms % 1000)
}

/// Whether end-call's answer says the call is over: an `ok` that is true,
/// at the top or in `result`, and neither of them false.
pub(crate) fn ended_ok(body: &[u8]) -> bool {
    let Ok(answer) = serde_json::from_slice::<Value>(body) else { return false };
    let oks = [answer.get("ok"), answer.pointer("/result/ok")].map(|ok| ok.and_then(Value::as_bool));
    oks.contains(&Some(true)) && !oks.contains(&Some(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(event: Value) -> Down {
        parse(event.to_string().as_bytes())
    }

    fn base64(bytes: &[u8]) -> String {
        let mut out = Vec::new();
        ws::base64(bytes, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_opening_records_are_these_bytes() {
        assert_eq!(
            admission("c-1", "homelink-0\"1"),
            br#"{"type":"hatch.voice.call.start","call_id":"c-1","device":"mcu-wearable","device_id":"homelink-0\"1","call_media_type":"audio","proactive_greeting_enabled":false}
"#
        );
        let offer: Value = serde_json::from_slice(CAPABILITY_OFFER).unwrap();
        assert_eq!(offer, json!({"type": "hatch.voice.capability.offer", "end_call_stop": 1}));
        assert!(CAPABILITY_OFFER.ends_with(b"}\n"));
    }

    #[test]
    fn a_frame_goes_up_as_2560_characters_that_read_back_the_same() {
        let pcm: Vec<i16> = (0..960).map(|i| (i * 67 - 32_000) as i16).chain([i16::MIN, i16::MAX]).take(960).collect();
        let mut out = Vec::from(&b"left over"[..]);
        append(&pcm, &mut out);
        assert!(out.starts_with(br#"{"type":"input_audio_buffer.append","audio":""#) && out.ends_with(b"\"}\n"));
        let record: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(record.as_object().unwrap().len(), 2, "no send_timestamp_ms");
        let audio = record["audio"].as_str().unwrap();
        assert_eq!(audio.len(), 2560);
        let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        assert_eq!(audio, base64(&bytes));
        assert_eq!(line(json!({"type": "response.output_audio.delta", "delta": audio})), Down::Audio { response_id: None, pcm });
    }

    #[test]
    fn a_cancel_names_its_response_escaped() {
        assert_eq!(cancel(&id("r\"1")), b"{\"type\":\"response.cancel\",\"response_id\":\"r\\\"1\"}\n");
    }

    #[test]
    fn end_call_says_when_in_milliseconds_and_the_thread_only_when_known() {
        let body: Value = serde_json::from_slice(&end_call_body("c-1", 1_793_293_200_000, 0, None)).unwrap();
        assert_eq!(
            body,
            json!({"call_id": "c-1", "call_started_at": "2026-10-29T17:00:00.000Z", "call_ended_at": "2026-10-29T17:00:00.001Z"}),
            "an end at least 1 ms after the start"
        );
        let body: Value = serde_json::from_slice(&end_call_body("c-1", 1_793_293_200_000, 61_234, Some(&id("t-1")))).unwrap();
        assert_eq!(body["call_ended_at"], "2026-10-29T17:01:01.234Z");
        assert_eq!(body["thread_id"], "t-1");
    }

    #[test]
    fn rfc3339_matches_python_datetime() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_793_293_200_000), "2026-10-29T17:00:00.000Z");
        assert_eq!(rfc3339(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(rfc3339(951_868_799_999), "2000-02-29T23:59:59.999Z");
        assert_eq!(rfc3339(4_107_542_400_000), "2100-03-01T00:00:00.000Z");
    }

    #[test]
    fn end_call_is_acknowledged_only_by_an_ok_and_no_refusal() {
        assert!(ended_ok(br#"{"ok":true,"result":{"ok":true}}"#));
        assert!(ended_ok(br#"{"ok":true}"#));
        assert!(ended_ok(br#"{"result":{"ok":true}}"#));
        for refused in [&br#"{"ok":true,"result":{"ok":false}}"#[..], br#"{"ok":false,"result":{"ok":true}}"#, b"{}", b"[true]", b"not json"] {
            assert!(!ended_ok(refused), "{}", String::from_utf8_lossy(refused));
        }
    }

    #[test]
    fn readiness_needs_this_call_a_session_and_a_real_thread_if_any() {
        assert_eq!(
            line(json!({"type": "hatch.voice.session", "call_id": "c", "session_id": "s", "thread_id": "t"})),
            Down::Session { call_id: id("c"), session_id: id("s"), thread_id: Some(id("t")) }
        );
        assert_eq!(
            line(json!({"type": "hatch.voice.session", "call_id": "c", "session_id": "s", "thread_id": null})),
            Down::Session { call_id: id("c"), session_id: id("s"), thread_id: None }
        );
        for bad in [
            json!({"type": "hatch.voice.session", "call_id": "c"}),
            json!({"type": "hatch.voice.session", "call_id": "c", "session_id": ""}),
            json!({"type": "hatch.voice.session", "call_id": "c", "session_id": "  "}),
            json!({"type": "hatch.voice.session", "call_id": "c", "session_id": 7}),
            json!({"type": "hatch.voice.session", "session_id": "s"}),
            json!({"type": "hatch.voice.session", "call_id": "c", "session_id": "s", "thread_id": ""}),
        ] {
            assert_eq!(line(bad.clone()), Down::BadSession, "{bad}");
        }
    }

    #[test]
    fn ids_are_one_to_128_bytes_without_nul() {
        let speech = |item: Value| line(json!({"type": "input_audio_buffer.speech_started", "item_id": item}));
        assert_eq!(speech(json!("x".repeat(128))), Down::SpeechStarted(Some(id(&"x".repeat(128)))));
        for bad in [json!(""), json!("x".repeat(129)), json!("a\u{0}b"), json!(5), Value::Null] {
            assert_eq!(speech(bad.clone()), Down::SpeechStarted(None), "{bad}");
        }
        assert_eq!(line(json!({"type": "hatch.voice.turn_started", "item_id": "i", "submission_message_id": "m"})), Down::TurnStarted { submission: id("m"), item: id("i") });
        assert_eq!(line(json!({"type": "hatch.voice.turn_started", "item_id": "i", "submission_message_id": ""})), Down::BadTurn);
        assert_eq!(line(json!({"type": "hatch.voice.turn_settled", "response_message_id": "m"})), Down::TurnSettled(id("m")));
        assert_eq!(line(json!({"type": "hatch.voice.turn_settled"})), Down::Other);
    }

    #[test]
    fn reads_the_events_the_gate_and_lifecycle_act_on() {
        assert_eq!(line(json!({"type": "error", "error": {"code": "session_dead", "message": "secret"}})), Down::Error { dead: true });
        assert_eq!(line(json!({"type": "error", "error": {"code": "server_error"}})), Down::Error { dead: false });
        assert_eq!(line(json!({"type": "hatch.voice.switch_input_hold", "held": true})), Down::Hold(true));
        assert_eq!(line(json!({"type": "hatch.voice.switch_input_hold", "held": "yes"})), Down::Other);
        assert_eq!(line(json!({"type": "input_audio_buffer.speech_stopped", "item_id": "i"})), Down::SpeechStopped(Some(id("i"))));
        assert_eq!(line(json!({"type": "hatch.voice.input_suppressed", "item_id": "i"})), Down::InputSuppressed(Some(id("i"))));
        assert_eq!(line(json!({"type": "response.created", "response": {"id": "r"}})), Down::ResponseCreated(Some(id("r"))));
        assert_eq!(line(json!({"type": "response.done", "response": {"id": "r", "status": "cancelled"}})), Down::ResponseDone(Some(id("r"))));
        assert_eq!(line(json!({"type": "response.completed", "response": "r"})), Down::ResponseDone(None));
        assert_eq!(line(json!({"type": "session.created"})), Down::Other);
        assert_eq!(line(json!(["not", "an", "event"])), Down::Other);
        assert_eq!(parse(b"{\"type\":"), Down::Other);
    }

    #[test]
    fn goodbye_is_a_stop_control_whatever_its_call_id() {
        let control = |item: Value| line(json!({"type": "conversation.item.added", "item": item}));
        assert_eq!(control(json!({"type": "session_control", "name": " STOP ", "call_id": "ctl-9", "arguments": "{}"})), Down::Stop);
        assert_eq!(control(json!({"type": "session_control", "name": "stop"})), Down::Stop);
        assert_eq!(control(json!({"type": "session_control", "name": "pause"})), Down::Other);
        assert_eq!(control(json!({"type": "message", "name": "stop"})), Down::Other);
    }

    #[test]
    fn audio_comes_in_both_shapes_and_only_as_pcm16_mono_at_24k() {
        let pcm = [1i16, -2, 300];
        let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        let frames = base64(&bytes);
        let cavd = |delta: Value| line(json!({"type": "continuous_audio_video.delta", "response_id": "p1", "audio_delta": delta}));
        let audio = Down::Audio { response_id: Some(id("p1")), pcm: pcm.to_vec() };
        assert_eq!(cavd(json!({"audio_frames": frames, "format": {"type": "", "rate": 24000}, "num_channels": 1, "num_frames": 3})), audio);
        assert_eq!(cavd(json!({"audio_frames": frames, "format": {"type": "audio/pcm", "rate": 24000}})), audio);
        assert_eq!(cavd(json!({"audio_frames": frames})), audio);
        assert_eq!(cavd(json!({"audio_frames": frames, "format": null})), audio);
        assert_eq!(line(json!({"type": "response.output_audio.delta", "response_id": "p1", "delta": frames})), Down::Audio { response_id: None, pcm: pcm.to_vec() });
        assert_eq!(line(json!({"type": "continuous_audio_video.delta", "response_id": "p1"})), Down::Other, "a video-only delta");
        assert_eq!(cavd(Value::Null), Down::Other);

        for bad in [
            json!({"audio_frames": frames, "format": {"rate": 16000}}),
            json!({"audio_frames": frames, "format": {"type": "audio/opus", "rate": 24000}}),
            json!({"audio_frames": frames, "format": {"type": "audio/pcm"}}),
            json!({"audio_frames": frames, "num_channels": 2}),
            json!({"audio_frames": base64(&bytes[..5])}),
            json!({"audio_frames": "AAA!"}),
            json!({"audio_frames": 12}),
            json!("frames"),
            json!({"audio_frames": base64(&alloc::vec![0; LARGEST_AUDIO + 2])}),
        ] {
            assert_eq!(cavd(bad.clone()), Down::BadAudio, "{bad}");
        }
        assert_eq!(cavd(json!({"audio_frames": base64(&alloc::vec![0; LARGEST_AUDIO])})), Down::Audio { response_id: Some(id("p1")), pcm: alloc::vec![0; LARGEST_AUDIO / 2] }, "4 s exactly");
    }
}
