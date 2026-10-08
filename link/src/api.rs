//! The Muse device API: which VM to connect to, and new tokens for old.
//! Ported from the Muse Gadget SDK's muse_api.py.

use crate::channel::{Channel, Fault, Security};
use crate::http;
use crate::pace::Knock;
use crate::turn::{text, truthy};
use crate::url::Endpoint;
use crate::{Clock, Net};
use alloc::{format, string::String, string::ToString};
use serde_json::Value;
use zeroize::Zeroizing;

/// The VM a session is opened to, with the bearer its front door wants.
pub(crate) struct Vm {
    /// What names it in the session's address: its id, or its name without one.
    pub target: String,
    pub name: String,
    pub bearer: Zeroizing<String>,
    pub knock: Knock,
}

/// The default VM of a `fetch_vms` answer, else the first usable one.
pub(crate) fn pick_vm(body: &[u8]) -> Option<Vm> {
    let answer: Value = serde_json::from_slice(body).ok()?;
    if truthy(answer.get("error_title")) || truthy(answer.get("backend_error_code")) {
        return None;
    }
    let hint = |key| answer.get(key).and_then(Value::as_i64).unwrap_or(0);
    let knock = Knock::from_hints(hint("retry_after_ms"), hint("max_retry_count"));
    let usable = |vm: &&Value| (text(vm, "vm_ws_url").is_some() || text(vm, "vm_url").is_some()) && text(vm, "vm_auth_token").is_some();
    let list = answer.get("vm_list")?.as_array()?;
    let is_default = |vm: &&Value| truthy(vm.get("default"));
    let chosen = list.iter().filter(usable).find(is_default).or(list.iter().find(usable))?;
    let (id, name) = (text(chosen, "vm_id").unwrap_or_default(), text(chosen, "vm_name").unwrap_or_default());
    Some(Vm {
        target: if id.is_empty() { name } else { id }.into(),
        name: if name.is_empty() { id } else { name }.into(),
        bearer: Zeroizing::new(text(chosen, "vm_auth_token")?.into()),
        knock,
    })
}

/// The new access and refresh tokens of a refresh answer.
pub(crate) fn rotated(body: &[u8]) -> Option<(String, String)> {
    let answer: Value = serde_json::from_slice(body).ok()?;
    let tokens = answer.get("payload").filter(|payload| payload.is_object()).unwrap_or(&answer);
    Some((text(tokens, "access_token")?.into(), text(tokens, "refresh_token")?.into()))
}

fn call<N: Net>(
    net: &mut N,
    clock: &impl Clock,
    security: &Security,
    method: &str,
    to: &Endpoint,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<http::Response, Fault> {
    let mut channel = Channel::open(net, security, to)?;
    let response = http::request(&mut channel, clock, method, to, headers, body)?;
    if response.status != 200 {
        return Err(Fault::Refused(response.status));
    }
    Ok(response)
}

/// `Ok(None)` is an answer that leases no VM to this device.
pub(crate) fn fetch_vm<N: Net>(
    net: &mut N,
    clock: &impl Clock,
    security: &Security,
    api: &Endpoint,
    access_token: &str,
    user_agent: &str,
) -> Result<Option<Vm>, Fault> {
    let bearer = Zeroizing::new(format!("Bearer {access_token}"));
    let headers = [("Authorization", bearer.as_str()), ("X-API-Version", "1.0.0"), ("User-Agent", user_agent)];
    let response = call(net, clock, security, "GET", &api.at("/fetch_vms"), &headers, None)?;
    Ok(pick_vm(&response.body))
}

/// Trades the refresh token for a new pair. The old pair is dead from then on.
pub(crate) fn refresh<N: Net>(
    net: &mut N,
    clock: &impl Clock,
    security: &Security,
    api: &Endpoint,
    refresh_token: &str,
    device_id: &str,
    sdk_token: Option<&str>,
    user_agent: &str,
) -> Result<(String, String), Fault> {
    // Apps hand over refresh tokens that already carry the prefix, and
    // doubling it makes the server reject the token.
    let raw = refresh_token.rsplit(':').next().unwrap_or(refresh_token);
    let bearer = Zeroizing::new(format!("Bearer hatch_refresh:{raw}"));
    let mut request = serde_json::Map::new();
    request.insert("device_id".into(), device_id.into());
    if let Some(sdk_token) = sdk_token.filter(|token| !token.is_empty()) {
        request.insert("sdk_token".into(), sdk_token.into());
    }
    let request = Zeroizing::new(Value::Object(request).to_string());
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json"), ("User-Agent", user_agent)];
    let response = call(net, clock, security, "POST", &api.at("/device_token/refresh"), &headers, Some(request.as_bytes()))?;
    rotated(&response.body).ok_or(Fault::Protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_default_vm_and_reads_the_retry_hints() {
        let vm = pick_vm(
            br#"{"retry_after_ms":500,"max_retry_count":20,"vm_list":[
                {"vm_ws_url":"wss://a","vm_auth_token":"skip","vm_name":"other","vm_id":"vm0"},
                {"vm_ws_url":"wss://b","vm_auth_token":"","vm_name":"no token","vm_id":"vm9","default":true},
                {"vm_url":"https://c","vm_auth_token":"tok","vm_name":"mine","vm_id":"vm 1","default":true}]}"#,
        )
        .unwrap();
        assert_eq!((vm.target.as_str(), vm.name.as_str(), vm.bearer.as_str()), ("vm 1", "mine", "tok"));
        assert_eq!(vm.knock, Knock { every_ms: 500, times: 20 });
    }

    #[test]
    fn takes_the_first_usable_vm_without_a_default_and_none_from_an_error() {
        let vm = pick_vm(br#"{"vm_list":[{"vm_auth_token":"no address"},{"vm_ws_url":"wss://a","vm_auth_token":"t","vm_name":"only"}]}"#).unwrap();
        assert_eq!((vm.target.as_str(), vm.knock), ("only", Knock::default()));
        assert!(pick_vm(br#"{"vm_list":[]}"#).is_none());
        assert!(pick_vm(br#"{"error_title":"nope","vm_list":[{"vm_ws_url":"wss://a","vm_auth_token":"t"}]}"#).is_none());
        assert!(pick_vm(b"<html>").is_none());
    }

    #[test]
    fn reads_rotated_tokens_with_or_without_a_wrapper() {
        assert_eq!(rotated(br#"{"payload":{"access_token":"a","refresh_token":"r"}}"#), Some(("a".into(), "r".into())));
        assert_eq!(rotated(br#"{"access_token":"a","refresh_token":"r"}"#), Some(("a".into(), "r".into())));
        assert_eq!(rotated(br#"{"access_token":"a","refresh_token":""}"#), None);
    }
}
