//! Permission bits as the dashboard's JS reads them: `BigInt(bits) & PermissionFlagsBits.X`.

/// `PermissionFlagsBits` values the collection files use.
pub mod flags {
    pub const KICK_MEMBERS: u128 = 1 << 1;
    pub const BAN_MEMBERS: u128 = 1 << 2;
    pub const ADMINISTRATOR: u128 = 1 << 3;
    pub const MANAGE_CHANNELS: u128 = 1 << 4;
    pub const MANAGE_GUILD: u128 = 1 << 5;
    pub const VIEW_CHANNEL: u128 = 1 << 10;
    pub const SEND_MESSAGES: u128 = 1 << 11;
    pub const MANAGE_MESSAGES: u128 = 1 << 13;
    pub const MENTION_EVERYONE: u128 = 1 << 17;
    pub const MANAGE_ROLES: u128 = 1 << 28;
    pub const MANAGE_WEBHOOKS: u128 = 1 << 29;
    pub const MODERATE_MEMBERS: u128 = 1 << 40;
}

/// The bit of a `PermissionFlagsBits` key the collection files name.
pub fn flag_bit(name: &str) -> Option<u128> {
    Some(match name {
        "Administrator" => flags::ADMINISTRATOR,
        "ManageGuild" => flags::MANAGE_GUILD,
        "ManageRoles" => flags::MANAGE_ROLES,
        "BanMembers" => flags::BAN_MEMBERS,
        "KickMembers" => flags::KICK_MEMBERS,
        "ManageChannels" => flags::MANAGE_CHANNELS,
        "ManageWebhooks" => flags::MANAGE_WEBHOOKS,
        "MentionEveryone" => flags::MENTION_EVERYONE,
        "ModerateMembers" => flags::MODERATE_MEMBERS,
        "ManageMessages" => flags::MANAGE_MESSAGES,
        "ViewChannel" => flags::VIEW_CHANNEL,
        "SendMessages" => flags::SEND_MESSAGES,
        _ => return None,
    })
}

fn is_js_space(c: char) -> bool {
    bot_util::js::is_js_space(c)
}

/// `BigInt(s)` for a string, as its low 128 bits in two's complement (enough for any `& flag` test); None where the
/// JS throws a SyntaxError. Accepts what StringToBigInt accepts: surrounding white space, '' (0), an optional sign
/// before decimal digits, and 0x / 0o / 0b literals without a sign.
pub fn js_bigint_bits(s: &str) -> Option<u128> {
    let t = s.trim_matches(is_js_space);
    if t.is_empty() {
        return Some(0);
    }
    let (radix, digits, negative) = if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, rest, false)
    } else if let Some(rest) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
        (8, rest, false)
    } else if let Some(rest) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (2, rest, false)
    } else if let Some(rest) = t.strip_prefix('-') {
        (10, rest, true)
    } else if let Some(rest) = t.strip_prefix('+') {
        (10, rest, false)
    } else {
        (10, t, false)
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: u128 = 0;
    for c in digits.chars() {
        let d = c.to_digit(radix)?;
        acc = acc.wrapping_mul(u128::from(radix)).wrapping_add(u128::from(d));
    }
    Some(if negative { acc.wrapping_neg() } else { acc })
}

/// `(BigInt(bits) & flag) === flag`, false when `BigInt` throws.
pub fn has_flag(bits: &str, flag: u128) -> bool {
    js_bigint_bits(bits).is_some_and(|b| b & flag == flag)
}
