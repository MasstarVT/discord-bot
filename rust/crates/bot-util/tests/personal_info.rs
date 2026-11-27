//! Port of `test/personal-info.test.js` (the detector part). The cross-language sweep against the JS lives in
//! `feat-automod/tests/automod.rs`.

use bot_util::personal_info::{PERSONAL_INFO_DELETE_REASON, describe_personal_info, find_personal_info, redact_personal_info};

pub const HITS: &[(&str, &[&str])] = &[
    ("call me at 555-123-4567", &["phone"]),
    ("my number is (555) 123 4567", &["phone"]),
    ("+1 (555) 123-4567", &["phone"]),
    ("1-555-234-5678", &["phone"]),
    ("+44 7700 900123", &["phone"]),
    ("07700 900123", &["phone"]),
    ("06 12 34 56 78", &["phone"]),
    ("0412 345 678", &["phone"]),
    ("5552345678", &["phone"]),
    ("phone:555.234.5678", &["phone"]),
    ("5\u{200B}5\u{200B}5-123-4567", &["phone"]),
    ("\u{FF15}\u{FF15}\u{FF15}-123-4567", &["phone"]),
    ("I live at 123 Main St", &["address"]),
    ("4521 n main st apt 3", &["address"]),
    ("12 Oak Street, Springfield", &["address"]),
    ("he lives at 742 Evergreen Terrace", &["address"]),
    ("10 Downing Street", &["address"]),
    ("1600 Pennsylvania Avenue NW", &["address"]),
    ("221b Baker St.", &["address"]),
    ("350 W 21st St", &["address"]),
    ("email me john.doe@gmail.com", &["email"]),
    ("me@x.io", &["email"]),
    ("ssn 123-45-6789", &["ssn"]),
    ("card 4111 1111 1111 1111", &["card"]),
    ("4111111111111111", &["card"]),
    ("3782-822463-10005", &["card"]),
    ("dox: 742 Evergreen Terrace, 555-123-4567, homer@springfield.com", &["phone", "address", "email"]),
];

pub const MISSES: &[&str] = &[
    "I have 3 cats in the way", "5 miles down the road", "2 hard drive", "I bought 2 SSD drive", "I ran 5 laps around the court",
    "2 people way too loud", "I need 2 more st", "2 days ago dr", "room 12 then lane", "top 5 drives", "1 Way",
    "score 1234567890", "id 123456789012345678", "<@123456789012345678>", "<#123456789012345678>", "<t:1764200000:R>",
    "use <:pog:123456789012345678>", "https://x.com/5552345678", "https://example.com/a@b.com",
    "2024-11-26", "01.02.2024", "26/11/2024", "10:30 pm", "$2,345,678,901", "1,000,000", "1000000000", "I scored 9001 points",
    "call 911", "its 3 am", "version 0.12.34", "v1.2.3.4", "play.server.net 192.168.1.10", "join 51.79.12.34:25565",
    "my cpu is 3.5 ghz", "4111 1111 1111 1112", "order #5552345678", "999-99-9999", "000-12-3456",
    "@everyone hi", "@someone.else", "user@localhost", "a@b", "",
];

#[test]
fn finds_each_kind() {
    for (text, kinds) in HITS {
        assert_eq!(find_personal_info(text), kinds.to_vec(), "{text}");
    }
}

#[test]
fn leaves_everyday_text_alone() {
    for text in MISSES {
        assert!(find_personal_info(text).is_empty(), "{text}");
        assert_eq!(redact_personal_info(text), *text, "{text}");
    }
}

#[test]
fn redacts_and_describes() {
    assert_eq!(
        redact_personal_info("dox: 742 Evergreen Terrace, 555-123-4567, homer@springfield.com"),
        "dox: [street address], [phone number], [email address]"
    );
    assert_eq!(redact_personal_info("card 4111 1111 1111 1111 ok"), "card [card number] ok");
    assert_eq!(redact_personal_info("see <@123456789012345678> at 5552345678"), "see <@123456789012345678> at [phone number]");
    assert_eq!(describe_personal_info(&["phone", "email"]), "phone number, email address");
    assert_eq!(PERSONAL_INFO_DELETE_REASON, "automod: personal info");
}

#[test]
fn stays_fast_on_hostile_input() {
    let inputs = [
        format!("+1{}", "2 ".repeat(4000)),
        format!("0{}", "1".repeat(8000)),
        format!("1 {}", "abc ".repeat(2000)),
        format!("{}@", "a".repeat(60)).repeat(130),
        format!("{}-", "9".repeat(30)).repeat(260),
        format!("+123{}", "4".repeat(30)).repeat(230),
        format!("{}St", "12 ".repeat(3000)),
    ];
    for s in &inputs {
        let t = std::time::Instant::now();
        find_personal_info(s);
        redact_personal_info(s);
        assert!(t.elapsed().as_millis() < 2000, "slow on {}", &s[..20]);
    }
}
