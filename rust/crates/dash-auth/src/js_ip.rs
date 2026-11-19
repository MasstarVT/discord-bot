//! Node's `net.isIP`: the exact regular expressions of Node's `internal/net.js` (an IPv6 address may carry a zone id,
//! an IPv4 octet may not have a leading zero), run with ECMAScript semantics.

use std::sync::LazyLock;

use bot_util::jsre::JsRegex;

const V4_SEG: &str = "(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])";
const V6_SEG: &str = "(?:[0-9a-fA-F]{1,4})";

fn v4_str() -> String {
    format!("(?:{V4_SEG}\\.){{3}}{V4_SEG}")
}

static IPV4_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(&format!("^{}$", v4_str()), ""));

static IPV6_RE: LazyLock<JsRegex> = LazyLock::new(|| {
    let v4 = v4_str();
    let s = V6_SEG;
    let pattern = format!(
        "^(?:(?:{s}:){{7}}(?:{s}|:)|(?:{s}:){{6}}(?:{v4}|:{s}|:)|(?:{s}:){{5}}(?::{v4}|(?::{s}){{1,2}}|:)|\
(?:{s}:){{4}}(?:(?::{s}){{0,1}}:{v4}|(?::{s}){{1,3}}|:)|(?:{s}:){{3}}(?:(?::{s}){{0,2}}:{v4}|(?::{s}){{1,4}}|:)|\
(?:{s}:){{2}}(?:(?::{s}){{0,3}}:{v4}|(?::{s}){{1,5}}|:)|(?:{s}:){{1}}(?:(?::{s}){{0,4}}:{v4}|(?::{s}){{1,6}}|:)|\
(?::(?:(?::{s}){{0,5}}:{v4}|(?::{s}){{1,7}}|:)))(?:%[0-9a-zA-Z-.:]{{1,}})?$"
    );
    JsRegex::new(&pattern, "")
});

/// `net.isIPv4(s)`.
pub fn is_ipv4(s: &str) -> bool {
    IPV4_RE.test(s)
}

/// `net.isIPv6(s)` (a `%zone` suffix is allowed).
pub fn is_ipv6(s: &str) -> bool {
    IPV6_RE.test(s)
}

/// `net.isIP(s)`: 4, 6 or 0.
pub fn is_ip(s: &str) -> u8 {
    if is_ipv4(s) {
        4
    } else if is_ipv6(s) {
        6
    } else {
        0
    }
}
