//! Port of `src/utils/text.js`: text and number formatting helpers. Lengths are counted in UTF-16 code units, as in
//! JS, so a limit means the same thing on both sides. None of these functions panics.

use crate::js;
use unicode_normalization::UnicodeNormalization;

/// The default ellipsis of [`clamp_text`].
pub const ELLIPSIS: &str = "…";

/// Cuts a string to at most `max` UTF-16 units, ending with '…' when it was cut. Never splits a surrogate pair.
pub fn clamp_text(s: &str, max: usize) -> String {
    clamp_text_with(s, max, ELLIPSIS)
}

/// [`clamp_text`] with a custom ellipsis.
pub fn clamp_text_with(s: &str, max: usize, ellipsis: &str) -> String {
    if max == 0 {
        return String::new();
    }
    if js::utf16_len(s) <= max {
        return s.to_string();
    }
    let ell_len = js::utf16_len(ellipsis);
    if max <= ell_len {
        return js::utf16_prefix(ellipsis, max).to_string();
    }
    // utf16_prefix never keeps half a surrogate pair, which is exactly the JS fix-up.
    let cut = js::utf16_prefix(s, max - ell_len);
    let cut = js::trim_end(cut);
    format!("{cut}{ellipsis}")
}

/// `clampText` with JS's numeric `max` rules: `floor(max)`, a non-finite max returns the input, `<= 0` gives ''.
pub fn clamp_text_f64(s: &str, max: f64, ellipsis: &str) -> String {
    let limit = max.floor();
    if !limit.is_finite() {
        return s.to_string();
    }
    if limit <= 0.0 {
        return String::new();
    }
    clamp_text_with(s, limit.min(usize::MAX as f64) as usize, ellipsis)
}

/// The text a member makes visible: the message's own content plus the content of every forwarded snapshot, joined
/// with '\n' and skipping empty parts (`effectiveText`; the caller passes the snapshot contents).
pub fn effective_text<'a>(content: Option<&str>, snapshot_contents: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut parts: Vec<&str> = vec![content.unwrap_or("")];
    parts.extend(snapshot_contents.into_iter().map(|s| s.unwrap_or("")));
    parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Packs lines greedily into chunks joined with '\n', each at most `max_len` long (an overlong line is clamped first).
pub fn chunk_lines<S: AsRef<str>>(lines: &[S], max_len: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current: Option<String> = None;
    for raw in lines {
        let line = clamp_text(raw.as_ref(), max_len);
        match current.take() {
            None => current = Some(line),
            Some(mut cur) => {
                if js::utf16_len(&cur) + 1 + js::utf16_len(&line) <= max_len {
                    cur.push('\n');
                    cur.push_str(&line);
                    current = Some(cur);
                } else {
                    chunks.push(cur);
                    current = Some(line);
                }
            }
        }
    }
    if let Some(cur) = current {
        chunks.push(cur);
    }
    chunks
}

/// The default "more" text of [`join_with_limit`].
pub fn default_more(n: usize) -> String {
    format!("… and {n} more")
}

/// Joins whole items with `sep`, appending `more(omitted)` when some had to be left out; the result is at most `max`
/// UTF-16 units and never cuts an item (a mention stays whole).
pub fn join_with_limit<S: AsRef<str>>(items: &[S], sep: &str, max: usize, more: impl Fn(usize) -> String) -> String {
    let strs: Vec<&str> = items.iter().map(AsRef::as_ref).collect();
    if strs.is_empty() {
        return String::new();
    }
    let whole = strs.join(sep);
    if js::utf16_len(&whole) <= max {
        return whole;
    }
    let sep_len = js::utf16_len(sep);
    let mut kept: Vec<&str> = Vec::new();
    let mut len = 0usize;
    for (i, s) in strs.iter().enumerate() {
        let next_len = len + if kept.is_empty() { 0 } else { sep_len } + js::utf16_len(s);
        let remaining = strs.len() - (i + 1);
        let tail = if remaining > 0 { sep_len + js::utf16_len(&more(remaining)) } else { 0 };
        if next_len + tail > max {
            break;
        }
        kept.push(s);
        len = next_len;
    }
    while !kept.is_empty()
        && kept.len() < strs.len()
        && js::utf16_len(&format!("{}{sep}{}", kept.join(sep), more(strs.len() - kept.len()))) > max
    {
        kept.pop();
    }
    if kept.len() == strs.len() {
        return kept.join(sep);
    }
    if kept.is_empty() {
        return clamp_text(&more(strs.len()), max);
    }
    format!("{}{sep}{}", kept.join(sep), more(strs.len() - kept.len()))
}

/// [`join_with_limit`] with the JS defaults (", ", 1024, "… and N more").
pub fn join_with_limit_default<S: AsRef<str>>(items: &[S]) -> String {
    join_with_limit(items, ", ", 1024, default_more)
}

/// en-US grouping with at most two fraction digits; a non-finite number gives '0' (`formatNumber`).
pub fn format_number(n: f64) -> String {
    if n.is_finite() { js::to_locale_en_us(n, 2) } else { "0".into() }
}

/// en-US grouping of a big integer (`formatNumber` of a BigInt).
pub fn format_bigint(n: i128) -> String {
    js::group_integer_text(&n.to_string())
}

/// English ordinal with en-US grouping: 1st, 2nd, 3rd, 11th, 22nd, 1,001st. A non-finite number gives its JS string.
pub fn ordinal(n: f64) -> String {
    let v = n.trunc();
    if !v.is_finite() {
        return js::number_to_string(n);
    }
    let a = v.abs();
    let mod100 = a % 100.0;
    let mut suffix = "th";
    if !(11.0..=13.0).contains(&mod100) {
        let mod10 = a % 10.0;
        if mod10 == 1.0 {
            suffix = "st";
        } else if mod10 == 2.0 {
            suffix = "nd";
        } else if mod10 == 3.0 {
            suffix = "rd";
        }
    }
    format!("{}{suffix}", format_number(v))
}

/// `Math.round`: nearest integer, ties toward +∞.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    if x - f >= 0.5 { f + 1.0 } else { f }
}

const COMPACT_UNITS: [(&str, f64); 4] = [("K", 1e3), ("M", 1e6), ("B", 1e9), ("T", 1e12)];

/// Short number: 999 → '999', 1500 → '1.5K', 999_950 → '1M' (never '1000.0K').
pub fn compact_number(n: f64) -> String {
    if !n.is_finite() {
        return "0".into();
    }
    let abs = n.abs();
    if abs < 1000.0 {
        return js::number_to_string(n.trunc());
    }
    let mut idx = 0;
    for i in (0..COMPACT_UNITS.len()).rev() {
        if abs >= COMPACT_UNITS[i].1 {
            idx = i;
            break;
        }
    }
    let mut r = js_round((n / COMPACT_UNITS[idx].1) * 10.0) / 10.0;
    if r.abs() >= 1000.0 && idx < COMPACT_UNITS.len() - 1 {
        idx += 1;
        r = js_round((n / COMPACT_UNITS[idx].1) * 10.0) / 10.0;
    }
    let s = js::number_to_string(r);
    let s = s.strip_suffix(".0").map(str::to_string).unwrap_or(s);
    format!("{s}{}", COMPACT_UNITS[idx].0)
}

/// Text progress bar, e.g. '▰▰▰▱▱▱▱▱▱▱' (`fraction` clamped to 0..1, NaN gives 0; `width` clamped to 1..100).
pub fn progress_bar(fraction: f64, width: f64, filled: &str, empty: &str) -> String {
    let mut f = if fraction.is_nan() { 0.0 } else { fraction };
    f = f.clamp(0.0, 1.0);
    let mut w = width.trunc();
    if !w.is_finite() {
        w = 10.0;
    }
    let w = w.clamp(1.0, 100.0);
    let k = js_round(f * w);
    format!("{}{}", filled.repeat(k as usize), empty.repeat((w - k) as usize))
}

/// [`progress_bar`] with the JS defaults (width 10, '▰' and '▱').
pub fn progress_bar_default(fraction: f64) -> String {
    progress_bar(fraction, 10.0, "▰", "▱")
}

/// Normalises a user-supplied name for use as a key: lowercase, accents removed, whitespace → '-', only
/// `[a-z0-9_-]` kept, at most `max` characters. An empty result gives `fallback`.
pub fn sanitize_name(s: &str, max: usize, fallback: &str) -> String {
    let decomposed: String = s.nfkd().filter(|c| !('\u{300}'..='\u{36f}').contains(c)).collect();
    let lowered = decomposed.to_lowercase();
    let trimmed = js::trim(&lowered);
    // \s+ → '-'
    let mut dashed = String::with_capacity(trimmed.len());
    let mut in_space = false;
    for c in trimmed.chars() {
        if js::is_js_space(c) {
            if !in_space {
                dashed.push('-');
            }
            in_space = true;
        } else {
            dashed.push(c);
            in_space = false;
        }
    }
    let kept: String = dashed.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-').collect();
    // -{2,} → '-'
    let mut collapsed = String::with_capacity(kept.len());
    for c in kept.chars() {
        if c == '-' && collapsed.ends_with('-') {
            continue;
        }
        collapsed.push(c);
    }
    let stripped = collapsed.trim_matches('-');
    let cut: String = stripped.chars().take(max).collect();
    let out = cut.trim_end_matches('-');
    if out.is_empty() { fallback.to_string() } else { out.to_string() }
}

/// [`sanitize_name`] with the JS defaults (32, 'item').
pub fn sanitize_name_default(s: &str) -> String {
    sanitize_name(s, 32, "item")
}

/// Whole percentages of each count in the sum, by the largest-remainder method (ties: the larger count, then the
/// earlier option). They add up to 100 when anyone voted and are all 0 otherwise. Counts are truncated and negative
/// or non-finite ones count as 0.
pub fn percentages(counts: &[f64]) -> Vec<i64> {
    let list: Vec<f64> = counts
        .iter()
        .map(|c| {
            let t = c.trunc();
            if t.is_nan() || t == 0.0 { 0.0 } else { t.max(0.0) }
        })
        .collect();
    let sum: f64 = list.iter().sum();
    if sum == 0.0 {
        return list.iter().map(|_| 0).collect();
    }
    let mut out: Vec<f64> = list.iter().map(|c| ((c * 100.0) / sum).floor()).collect();
    let mut order: Vec<(usize, f64, f64)> = list.iter().enumerate().map(|(i, &c)| (i, c, (c * 100.0) % sum)).collect();
    order.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.cmp(&b.0))
    });
    let mut left = 100.0 - out.iter().sum::<f64>();
    let mut k = 0;
    while left > 0.0 && k < order.len() {
        out[order[k].0] += 1.0;
        left -= 1.0;
        k += 1;
    }
    out.into_iter().map(|x| x as i64).collect()
}
