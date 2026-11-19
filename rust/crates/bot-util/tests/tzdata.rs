//! chrono-tz ships tzdata 2025b while Node's ICU had 2026b when the goldens were made. This compares the offset of
//! every id Intl accepts (1054 spellings) on the 1st and 15th of each month 1970-2014 and every day 2015-2040 with
//! what Node printed. Zones whose rules changed since 2025b (America/Vancouver, America/Tijuana) and the SystemV
//! zones with daylight saving carry ICU's own transition table in data/tz.json, so they match too.

mod common;

use serde_json::Value;

/// Ids still allowed to differ (none: every difference found is covered by an ICU table in data/tz.json).
const KNOWN_TZDATA_DIFFERENCES: &[&str] = &[];

#[test]
fn every_zone_offset_matches_node_1970_to_2040() {
    let text = std::fs::read_to_string(common::golden_dir().join("tz-offsets.json")).expect("tz-offsets.json");
    let v: Value = serde_json::from_str(&text).unwrap();
    let samples: Vec<f64> = v["samples"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
    let mut differing = Vec::new();
    for (zone, changes) in v["offsets"].as_object().unwrap() {
        let changes: Vec<(usize, i64)> = changes.as_array().unwrap().iter().map(|c| (c[0].as_u64().unwrap() as usize, c[1].as_i64().unwrap())).collect();
        let mut expected = 0;
        let mut k = 0;
        let mut first_bad = None;
        for (i, &t) in samples.iter().enumerate() {
            while k < changes.len() && changes[k].0 <= i {
                expected = changes[k].1;
                k += 1;
            }
            let got = bot_util::time::offset_seconds_at(t, zone).map(i64::from);
            if got != Some(expected) && first_bad.is_none() {
                first_bad = Some((t, expected, got));
            }
        }
        if let Some((t, want, got)) = first_bad {
            if !KNOWN_TZDATA_DIFFERENCES.contains(&zone.as_str()) {
                differing.push(format!("{zone}: at {} Node {want}s, Rust {got:?}", bot_util::js::to_iso_string(t).unwrap_or_default()));
            }
        }
    }
    assert!(differing.is_empty(), "{} zones differ:\n{}", differing.len(), differing.join("\n"));
}
