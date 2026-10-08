//! One question and the reply that adds up to it, out of the events on the
//! chat stream.

use alloc::{string::String, string::ToString, vec::Vec};
use serde_json::Value;

const MOST_WAITING_EVENTS: usize = 256;
const MOST_REPLY_MESSAGES: usize = 32;
const LONGEST_REPLY: usize = 64 * 1024;

/// Python's idea of true, which is what the reference client tests.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// A field that is a string with something in it. Anything else is absent.
pub(crate) fn text<'a>(object: &'a Value, key: &str) -> Option<&'a str> {
    object.get(key)?.as_str().filter(|text| !text.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    User,
    Start,
    Append,
    Done,
    Assistant,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Message {
    kind: Kind,
    id: String,
    parent: Option<String>,
    /// The words to add for an append, the whole text for the others.
    text: Option<String>,
    /// False only when the event says outright its text is not final yet.
    ready: bool,
}

/// A command for this device, sent on the chat stream because the turn it
/// belongs to started here.
#[derive(Debug, PartialEq)]
pub(crate) struct Invoke {
    /// The id to answer with, as JSON.
    pub id: String,
    pub command: String,
    /// A JSON object.
    pub params: String,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Line {
    Invoke(Invoke),
    /// Muse saying whether it is still working.
    Busy(bool),
    Message(Message),
    /// An event this client does not read. It still shows the stream is alive.
    Unread,
    /// Not an event at all.
    Other,
}

impl Line {
    pub(crate) fn parse(line: &[u8]) -> Line {
        let Ok(event) = serde_json::from_slice::<Value>(line) else { return Line::Other };
        // Lines of any other type acknowledge the subscription itself.
        if text(&event, "type") != Some("event") {
            return Line::Other;
        }
        let nothing = Value::Null;
        let payload = event.get("payload").filter(|payload| payload.is_object()).unwrap_or(&nothing);
        let kind = match text(&event, "event") {
            Some("client.invoke") => {
                return match text(payload, "invoke_id") {
                    None => Line::Unread,
                    Some(id) => Line::Invoke(Invoke {
                        id: Value::from(id).to_string(),
                        command: text(payload, "command_id").unwrap_or_default().into(),
                        params: text(payload, "params_json")
                            .filter(|params| serde_json::from_str::<Value>(params).is_ok_and(|params| params.is_object()))
                            .unwrap_or("{}")
                            .into(),
                    }),
                };
            }
            Some("agent.status" | "task.status") => {
                let (activity, status) = (payload.get("activity_code"), payload.get("status"));
                let named = |value: Option<&Value>, idle: [&str; 2]| !idle.contains(&value.and_then(Value::as_str).unwrap_or(""));
                return if truthy(activity) {
                    Line::Busy(named(activity, ["online", "idle"]))
                } else if truthy(status) {
                    Line::Busy(named(status, ["completed", "failed"]))
                } else {
                    Line::Unread
                };
            }
            Some("message.user") => Kind::User,
            Some("delta.message_start") => Kind::Start,
            Some("delta.text_append") => Kind::Append,
            Some("delta.message_done") => Kind::Done,
            Some("message.assistant") => Kind::Assistant,
            _ => return Line::Unread,
        };
        let id = text(payload, "message_id").or(text(&event, "message_id")).or(text(payload, "id")).unwrap_or_default();
        let words = match kind {
            Kind::Append => text(payload, "text"),
            _ => text(payload, "display_text").or(text(payload, "content")),
        };
        Line::Message(Message {
            kind,
            id: id.into(),
            parent: text(payload, "reply_to_message_id").or(text(payload, "parent_message_id")).map(String::from),
            text: words.map(String::from),
            ready: payload.get("display_text_ready") != Some(&Value::Bool(false)),
        })
    }
}

struct Reply {
    id: String,
    text: String,
    done: bool,
}

/// Muse does not link its reply to the question: a reply names no parent and
/// its text chunks name themselves. So a message that starts after ours
/// belongs to the turn, where "after ours" is after the stream echoes our
/// message, or after the acknowledgment if the echo came first.
pub(crate) struct Turn {
    message_id: Option<String>,
    ours_seen: bool,
    /// Events race the acknowledgment that names our message, so the first
    /// few wait here for it.
    waiting: Vec<Message>,
    replies: Vec<Reply>,
    busy: bool,
    /// What Muse heard in a voice note.
    pub heard: String,
    pub last_event_ms: u64,
    heard_fresh: bool,
    /// The reply changed since `take_change`.
    changed: bool,
}

fn push_capped(into: &mut String, words: &str) {
    let mut room = (LONGEST_REPLY - into.len()).min(words.len());
    while !words.is_char_boundary(room) {
        room -= 1;
    }
    into.push_str(&words[..room]);
}

impl Turn {
    pub(crate) fn new(now_ms: u64) -> Turn {
        Turn {
            message_id: None,
            ours_seen: false,
            waiting: Vec::new(),
            replies: Vec::new(),
            busy: false,
            heard: String::new(),
            last_event_ms: now_ms,
            heard_fresh: false,
            changed: false,
        }
    }

    pub(crate) fn busy(&mut self, busy: bool, now_ms: u64) {
        self.last_event_ms = now_ms;
        self.busy = busy;
    }

    pub(crate) fn add(&mut self, message: Message, now_ms: u64) {
        self.last_event_ms = now_ms;
        if self.message_id.is_some() {
            self.apply(message, true);
        } else if self.waiting.len() < MOST_WAITING_EVENTS {
            self.waiting.push(message);
        }
    }

    /// Says which message was ours and replays what arrived before that was
    /// known.
    pub(crate) fn acknowledged(&mut self, message_id: &str) {
        self.message_id = Some(message_id.into());
        self.ours_seen = false;
        for message in core::mem::take(&mut self.waiting) {
            self.apply(message, false);
        }
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.replies.iter().position(|reply| reply.id == id)
    }

    /// `after_ack` is whether the event arrived after the acknowledgment,
    /// when any new parentless message is taken to follow ours.
    fn apply(&mut self, message: Message, after_ack: bool) {
        let ours = self.message_id.as_deref();
        if message.kind == Kind::User {
            if Some(message.id.as_str()) == ours {
                self.ours_seen = true;
                // The stored message carries the words, which for a voice
                // note are the transcript.
                let heard = transcript(message.text.as_deref().unwrap_or_default());
                if message.ready && !heard.is_empty() && heard != self.heard {
                    self.heard = heard;
                    self.heard_fresh = true;
                }
            }
            return;
        }
        let at = match self.find(&message.id) {
            Some(at) => at,
            None => {
                let belongs = match message.parent.as_deref() {
                    Some(parent) => Some(parent) == ours || self.find(parent).is_some(),
                    None => self.ours_seen || after_ack,
                };
                if message.kind == Kind::Append || !belongs || self.replies.len() == MOST_REPLY_MESSAGES {
                    return;
                }
                self.replies.push(Reply { id: message.id, text: String::new(), done: false });
                self.replies.len() - 1
            }
        };
        let reply = &mut self.replies[at];
        self.changed = true;
        match message.kind {
            Kind::Start | Kind::User => {}
            Kind::Append => push_capped(&mut reply.text, message.text.as_deref().unwrap_or_default()),
            Kind::Done | Kind::Assistant => {
                if let Some(whole) = message.text {
                    reply.text.clear();
                    push_capped(&mut reply.text, &whole);
                }
                reply.done |= message.kind == Kind::Done || message.ready;
            }
        }
    }

    /// True once after each change to the reply.
    pub(crate) fn take_change(&mut self) -> bool {
        core::mem::take(&mut self.changed)
    }

    /// What Muse heard, once after each time it changes.
    pub(crate) fn take_heard(&mut self) -> Option<&str> {
        core::mem::take(&mut self.heard_fresh).then_some(self.heard.as_str())
    }

    /// Complete as far as anyone can tell: every reply message is done and
    /// Muse is not working on more. Nothing on the stream marks the end of a
    /// turn, so this and a pause are all there is.
    pub(crate) fn whole(&self) -> bool {
        !self.busy && !self.replies.is_empty() && self.replies.iter().all(|reply| reply.done)
    }

    /// The reply so far: every message that has words, a blank line between.
    pub(crate) fn write_text(&self, out: &mut String) {
        out.clear();
        for reply in self.replies.iter().filter(|reply| !reply.text.is_empty()) {
            let gap = if out.is_empty() { "" } else { "\n\n" };
            if out.len() + gap.len() + reply.text.len() > LONGEST_REPLY {
                break;
            }
            out.push_str(gap);
            out.push_str(&reply.text);
        }
    }
}

/// What was said, out of a stored voice note's text. Muse follows the words
/// with a line naming the recording, like
/// "[file:audio/wav workspace/user/files/voice_note-64.wav]".
pub(crate) fn transcript(display: &str) -> String {
    let mut said = String::new();
    for line in display.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with("[file:")) {
        if !said.is_empty() {
            said.push(' ');
        }
        said.push_str(line);
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str, payload: &str) -> Line {
        Line::parse(format!(r#"{{"type":"event","event":"{name}","payload":{payload}}}"#).as_bytes())
    }

    fn feed(turn: &mut Turn, name: &str, payload: &str) {
        match event(name, payload) {
            Line::Message(message) => turn.add(message, 0),
            Line::Busy(busy) => turn.busy(busy, 0),
            other => panic!("{name} parsed as {other:?}"),
        }
    }

    fn shown(turn: &Turn) -> String {
        let mut out = String::new();
        turn.write_text(&mut out);
        out
    }

    #[test]
    fn transcript_drops_the_line_naming_the_recording() {
        assert_eq!(transcript("turn the volume up\n[file:audio/wav workspace/user/files/voice_note-64.wav]"), "turn the volume up");
        assert_eq!(transcript("  one \n\n two\r\n[file:x]\n"), "one two");
        assert_eq!(transcript("[file:audio/wav a.wav]"), "");
        assert_eq!(transcript("a [file:kept] b"), "a [file:kept] b");
    }

    #[test]
    fn the_reply_is_the_message_that_starts_after_ours() {
        // The shapes the real service sends, in the order the fake sends them:
        // the events race the acknowledgment.
        let mut turn = Turn::new(0);
        feed(&mut turn, "delta.message_start", r#"{"message_id":"earlier","reply_to_message_id":""}"#);
        feed(&mut turn, "delta.text_append", r#"{"message_id":"earlier","text":"not for this device"}"#);
        feed(&mut turn, "message.user", r#"{"message_id":"u1","display_text":"hello\n[file:audio/wav x.wav]"}"#);
        feed(&mut turn, "agent.status", r#"{"activity_code":"working"}"#);
        feed(&mut turn, "delta.message_start", r#"{"message_id":"a1","reply_to_message_id":""}"#);
        feed(&mut turn, "delta.text_append", r#"{"message_id":"a1","parent_message_id":"a1","text":"you said: "}"#);
        assert_eq!(shown(&turn), "", "nothing is ours until the acknowledgment names our message");
        turn.acknowledged("u1");
        assert_eq!(shown(&turn), "you said: ");
        assert_eq!(turn.take_heard(), Some("hello"));
        assert_eq!(turn.take_heard(), None);
        assert!(turn.take_change() && !turn.take_change());
        feed(&mut turn, "delta.text_append", r#"{"message_id":"a1","parent_message_id":"a1","text":"hello"}"#);
        feed(&mut turn, "agent.status", r#"{"activity_code":"online"}"#);
        assert!(!turn.whole(), "not done yet");
        feed(&mut turn, "delta.message_done", r#"{"message_id":"a1","status":"completed"}"#);
        assert!(turn.whole());
        assert_eq!(shown(&turn), "you said: hello");
    }

    #[test]
    fn a_second_message_joins_with_a_blank_line_and_final_text_replaces_chunks() {
        let mut turn = Turn::new(0);
        turn.acknowledged("u1");
        feed(&mut turn, "delta.message_start", r#"{"message_id":"a1"}"#);
        feed(&mut turn, "delta.text_append", r#"{"message_id":"a1","text":"dra"}"#);
        feed(&mut turn, "message.assistant", r#"{"message_id":"a1","display_text":"final","display_text_ready":false}"#);
        assert!(!turn.whole(), "the text is not final yet");
        feed(&mut turn, "message.assistant", r#"{"message_id":"a1","content":"final one"}"#);
        feed(&mut turn, "delta.message_start", r#"{"message_id":"a2","reply_to_message_id":"a1"}"#);
        assert!(!turn.whole());
        feed(&mut turn, "delta.text_append", r#"{"message_id":"a2","text":"two"}"#);
        feed(&mut turn, "delta.message_done", r#"{"message_id":"a2"}"#);
        feed(&mut turn, "delta.message_start", r#"{"message_id":"stray","reply_to_message_id":"someone-else"}"#);
        assert!(turn.whole());
        assert_eq!(shown(&turn), "final one\n\ntwo");
        feed(&mut turn, "task.status", r#"{"status":"running"}"#);
        assert!(!turn.whole(), "Muse says it is still working");
    }

    #[test]
    fn a_runaway_reply_is_cut_on_a_character() {
        let mut turn = Turn::new(0);
        turn.acknowledged("u1");
        feed(&mut turn, "delta.message_start", r#"{"message_id":"a1"}"#);
        let chunk = format!(r#"{{"message_id":"a1","text":"{}"}}"#, "é".repeat(20_000));
        feed(&mut turn, "delta.text_append", &chunk);
        feed(&mut turn, "delta.text_append", &chunk);
        let text = shown(&turn);
        assert!(text.len() <= LONGEST_REPLY && text.len() >= LONGEST_REPLY - 1 && text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn a_command_on_the_chat_stream_is_an_invoke() {
        let line = event(
            "client.invoke",
            r#"{"command_id":"volume","invoke_id":"inv-chat","params_json":"{\"level\":10}","timeout_ms":30000}"#,
        );
        assert_eq!(line, Line::Invoke(Invoke { id: r#""inv-chat""#.into(), command: "volume".into(), params: r#"{"level":10}"#.into() }));
        for not_an_object in ["null", "{broken", "[1]"] {
            let bare = event("client.invoke", &format!(r#"{{"command_id":"mute","invoke_id":"i2","params_json":"{not_an_object}"}}"#));
            assert_eq!(bare, Line::Invoke(Invoke { id: r#""i2""#.into(), command: "mute".into(), params: "{}".into() }));
        }
        assert_eq!(event("client.invoke", r#"{"command_id":"mute"}"#), Line::Unread, "nothing to answer to");
        assert_eq!(event("something.new", "{}"), Line::Unread);
        assert_eq!(Line::parse(br#"{"type":"ack"}"#), Line::Other);
        assert_eq!(Line::parse(b"not json"), Line::Other);
    }
}
