//! Port of `src/utils/fatalGateway.js`: gateway close codes that reconnecting can never fix.

/// One fatal close code: its name and the fix shown to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FatalCloseCode {
    pub code: u16,
    pub name: &'static str,
    pub fix: &'static str,
}

/// `FATAL_CLOSE_CODES`, in code order.
pub const FATAL_CLOSE_CODES: [FatalCloseCode; 3] = [
    FatalCloseCode {
        code: 4004,
        name: "Authentication failed",
        fix: "Discord rejected DISCORD_TOKEN. Reset the token in the Developer Portal (Bot → Reset Token), put the new one in DISCORD_TOKEN and restart the bot.",
    },
    FatalCloseCode {
        code: 4013,
        name: "Invalid intents",
        fix: "The intents sent to the gateway are invalid. Check the GatewayIntentBits list in src/bot.js, then restart the bot.",
    },
    FatalCloseCode {
        code: 4014,
        name: "Disallowed intents",
        fix: "Enable the privileged intents (Server Members, Presence and Message Content) under Bot → Privileged Gateway Intents in the Developer Portal, then restart the bot.",
    },
];

/// Key of the message a shard sends to the manager when it hits one of the codes above.
pub const FATAL_MESSAGE_KEY: &str = "_fatalGatewayClose";

/// The entry for a close code, if it is fatal.
pub fn fatal_close_code(code: u16) -> Option<&'static FatalCloseCode> {
    FATAL_CLOSE_CODES.iter().find(|c| c.code == code)
}

/// Maps a login failure to a fatal close code: an error code `TokenInvalid` (a 401 before the gateway) is 4004, and
/// the gateway's own messages map to 4004, 4013 and 4014. `None` for anything else.
pub fn fatal_code_from_error(code: Option<&str>, message: Option<&str>) -> Option<u16> {
    if code == Some("TokenInvalid") {
        return Some(4004);
    }
    match message {
        Some("Authentication failed") => Some(4004),
        Some("Used invalid intents") => Some(4013),
        Some("Used disallowed intents") => Some(4014),
        _ => None,
    }
}
