//! Client-side defaults Prisma used to apply: `cuid()`, `uuid()`, `now()` / `@updatedAt`.

use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

const BASE: u64 = 36;
const BLOCK: usize = 4;
const DISCRETE: u64 = 36 * 36 * 36 * 36; // 36^4

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % BASE) as usize]);
        n /= BASE;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Left-pads with zeros and keeps the last `size` characters (cuid's `pad`).
fn pad(s: &str, size: usize) -> String {
    if s.len() >= size {
        return s[s.len() - size..].to_string();
    }
    format!("{}{}", "0".repeat(size - s.len()), s)
}

fn random_block() -> String {
    let n: u64 = rand::random::<u64>() % DISCRETE;
    pad(&to_base36(n), BLOCK)
}

fn fingerprint() -> &'static str {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        let pid = pad(&to_base36(u64::from(std::process::id())), 2);
        let host = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")).unwrap_or_else(|_| "host".into());
        let sum: u64 = host.bytes().map(u64::from).sum::<u64>() + host.len() as u64 + BASE;
        format!("{pid}{}", pad(&to_base36(sum), 2))
    })
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A collision-resistant id in Prisma's `cuid()` format (cuid v1): 25 lowercase base-36 characters starting with `c`
/// (`c` + 8 timestamp + 4 counter + 4 fingerprint + 8 random).
pub fn new_cuid() -> String {
    let ts = pad(&to_base36(Utc::now().timestamp_millis().max(0) as u64), 8);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed) as u64 % DISCRETE;
    let counter = pad(&to_base36(count), BLOCK);
    format!("c{ts}{counter}{}{}{}", fingerprint(), random_block(), random_block())
}

/// `true` when `s` has the shape of a Prisma cuid.
pub fn is_cuid(s: &str) -> bool {
    s.len() == 25 && s.starts_with('c') && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// A random (v4) UUID string, lowercase and hyphenated, like Prisma's `uuid()` and `_prisma_migrations.id`.
pub fn new_uuid() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// The current UTC time at millisecond precision (what a JS `new Date()` holds and `timestamp(3)` stores).
pub fn now_utc() -> DateTime<Utc> {
    crate::serde_js::truncate_ms(Utc::now())
}
