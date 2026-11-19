//! Port of `src/utils/gatewayFeatures.js`: gateway features the operator can switch off in `.env`. Read on every call.

/// False when `DISABLE_PRESENCES` is 1, true or yes (any case, trimmed): the bot then connects without the privileged
/// Presence intent.
pub fn presences_enabled() -> bool {
    presences_enabled_from(std::env::var("DISABLE_PRESENCES").ok().as_deref())
}

/// [`presences_enabled`] for a given value of `DISABLE_PRESENCES`.
pub fn presences_enabled_from(value: Option<&str>) -> bool {
    let v = crate::js::trim(value.unwrap_or("")).to_lowercase();
    !matches!(v.as_str(), "1" | "true" | "yes")
}
