//! The pairing, in `state/` beside the app. The files and their fields are
//! the ones a Muse gadget client keeps, so tools/pair.py can hand over a
//! pairing made on another device.

use crate::files;
use alloc::{format, string::String};
use muse_link::{NotSaved, Pairing, Vault};
use serde_json::{Map, Value};

const PAIRING: &str = "state/pairing.json";
const PAIRING_NEW: &str = "state/pairing.json.new";
const IDENTITY: &str = "state/identity.json";
const SDK_TOKEN: &str = "state/sdk_token.txt";

/// The pairing file as read, so a save writes back every field it held and
/// not only the three this app knows.
pub struct Store {
    fields: Map<String, Value>,
    /// Whether the next save follows a renewal that carried the SDK token.
    reporting_sdk_token: bool,
}

pub struct Saved {
    pub store: Store,
    pub pairing: Pairing,
    pub api_root: Option<String>,
    pub noise_host: Option<String>,
    pub node_id: String,
    /// The SDK token, while Muse has yet to be told it. Reporting it costs
    /// a renewal at every start, and Muse needs it once.
    pub sdk_token: Option<String>,
}

fn object(path: &str) -> Option<Map<String, Value>> {
    match serde_json::from_slice(&files::read(path)?).ok()? {
        Value::Object(fields) => Some(fields),
        _ => None,
    }
}

pub fn load() -> Result<Saved, String> {
    // A save writes the .new file and then swaps it in. If the power went
    // between those steps, the .new file is the live pairing.
    let fields = object(PAIRING_NEW)
        .filter(|fields| fields.contains_key("refresh_token"))
        .or_else(|| object(PAIRING))
        .ok_or("no pairing on the Memory Stick")?;
    let text = |name: &str| fields.get(name).and_then(Value::as_str).filter(|text| !text.is_empty()).map(String::from);
    let pairing = Pairing {
        access_token: text("access_token").ok_or("the pairing has no access token")?,
        refresh_token: text("refresh_token").ok_or("the pairing has no refresh token")?,
        saved_at: fields.get("access_token_saved_at").and_then(Value::as_f64).map_or(0, |seconds| seconds as u64),
    };
    let identity = object(IDENTITY).ok_or("no identity on the Memory Stick")?;
    let mac = identity.get("mac").and_then(Value::as_str).ok_or("the identity has no address")?;
    let hex: String = mac.chars().filter(|&c| c != ':').take(12).collect();
    // The node id is "homelink-" plus the last six hex digits of the identity.
    let node_id = format!("homelink-{}", &hex[hex.len().saturating_sub(6)..]);
    let reported = fields.get("sdk_token_reported").and_then(Value::as_bool).unwrap_or(false);
    let sdk_token = files::read(SDK_TOKEN)
        .and_then(|file| String::from_utf8(file).ok())
        .map(|token| String::from(token.trim()))
        .filter(|token| !reported && !token.is_empty());
    let (api_root, noise_host) = (text("api_url_v2"), text("noise_host"));
    let store = Store { fields, reporting_sdk_token: sdk_token.is_some() };
    Ok(Saved { api_root, noise_host, node_id, pairing, sdk_token, store })
}

impl Vault for Store {
    fn save(&mut self, pairing: &Pairing) -> Result<(), NotSaved> {
        self.fields.insert("access_token".into(), pairing.access_token.as_str().into());
        self.fields.insert("refresh_token".into(), pairing.refresh_token.as_str().into());
        self.fields.insert("access_token_saved_at".into(), pairing.saved_at.into());
        if self.reporting_sdk_token {
            self.fields.insert("sdk_token_reported".into(), true.into());
        }
        let text = serde_json::to_string(&self.fields).map_err(|_| NotSaved)?;
        if !files::write(PAIRING_NEW, text.as_bytes()) || files::read(PAIRING_NEW).as_deref() != Some(text.as_bytes()) {
            return Err(NotSaved);
        }
        // The new tokens are safe from here on: `load` prefers the .new file,
        // so a swap that fails or is cut short loses nothing.
        files::remove(PAIRING);
        files::rename(PAIRING_NEW, PAIRING);
        Ok(())
    }
}
