//! Where a request goes: the pieces of an http, https, ws or wss address.

use alloc::string::String;
use core::fmt::Write;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Endpoint {
    /// https or wss.
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query. Always starts with a slash.
    pub path: String,
}

impl Endpoint {
    pub(crate) fn parse(url: &str) -> Option<Endpoint> {
        let (secure, rest) = [("https://", true), ("http://", false), ("wss://", true), ("ws://", false)]
            .into_iter()
            .find_map(|(scheme, secure)| Some((secure, url.strip_prefix(scheme)?)))?;
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        let (host, port) = match authority.split_once(':') {
            None => (authority, if secure { 443 } else { 80 }),
            Some((host, digits)) => {
                let plain = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
                (host, digits.parse().ok().filter(|&port| plain && port != 0)?)
            }
        };
        if host.is_empty() || host.len() > 255 || tail.len() > 1023 {
            return None;
        }
        let mut path = String::with_capacity(tail.len() + 1);
        if !tail.starts_with('/') {
            path.push('/');
        }
        path.push_str(tail);
        Some(Endpoint { secure, host: host.into(), port, path })
    }

    /// The same place with another path.
    pub(crate) fn at(&self, path: &str) -> Endpoint {
        Endpoint { path: path.into(), ..self.clone() }
    }

    /// The Host header: the port is named only when it is not the scheme's own.
    pub(crate) fn authority(&self) -> String {
        let mut text = self.host.clone();
        if self.port != if self.secure { 443 } else { 80 } {
            let _ = write!(text, ":{}", self.port);
        }
        text
    }
}

/// Percent-encodes like JavaScript's encodeURIComponent.
pub(crate) fn encode_component(text: &str, out: &mut String) {
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(url: &str) -> Option<(bool, String, u16, String)> {
        Endpoint::parse(url).map(|e| (e.secure, e.host, e.port, e.path))
    }

    #[test]
    fn parses_each_scheme_with_its_default_port() {
        assert_eq!(parts("https://api.muse.ai"), Some((true, "api.muse.ai".into(), 443, "/".into())));
        assert_eq!(parts("http://a.b/x?y=1"), Some((false, "a.b".into(), 80, "/x?y=1".into())));
        assert_eq!(parts("wss://h:8443/v1/noise?vm_id=a"), Some((true, "h".into(), 8443, "/v1/noise?vm_id=a".into())));
        assert_eq!(parts("ws://127.0.0.1:9?q"), Some((false, "127.0.0.1".into(), 9, "/?q".into())));
    }

    #[test]
    fn rejects_what_it_cannot_reach() {
        for bad in ["ftp://a", "api.muse.ai", "https://", "https://:443/", "http://a:0/", "http://a:65536/", "http://a:+80/", "http://a:/"] {
            assert_eq!(Endpoint::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn host_header_names_only_an_unusual_port() {
        assert_eq!(Endpoint::parse("https://a.b:443/").unwrap().authority(), "a.b");
        assert_eq!(Endpoint::parse("http://a.b:8080/").unwrap().authority(), "a.b:8080");
        assert_eq!(Endpoint::parse("https://a.b/x").unwrap().at("/y").path, "/y");
    }

    #[test]
    fn encodes_a_query_value() {
        let mut out = String::new();
        encode_component("vm 1/é~*", &mut out);
        assert_eq!(out, "vm%201%2F%C3%A9~*");
    }
}
