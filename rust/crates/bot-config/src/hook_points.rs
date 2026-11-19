//! Port of `src/config/hookPoints.js`: the order plugin hooks run in, per event.

pub use crate::tables::{HOOK_ORDER, HookPoint, INTERNAL_HOOK_POINTS, SYNTHETIC_HOOK_POINTS};

crate::string_enum_impls!(HookPoint);

impl HookPoint {
    /// `HOOK_ORDER[event]`: hook ids in run order.
    pub fn order(self) -> &'static [&'static str] {
        // HOOK_ORDER is generated in the same order as the enum (checked by the tests).
        HOOK_ORDER[self as usize].1
    }

    /// True for `INTERNAL_HOOK_POINTS` (emitted by services).
    pub fn is_internal(self) -> bool {
        INTERNAL_HOOK_POINTS.contains(&self)
    }

    /// True for `SYNTHETIC_HOOK_POINTS` (emitted by runGuildReady).
    pub fn is_synthetic(self) -> bool {
        SYNTHETIC_HOOK_POINTS.contains(&self)
    }
}

/// `hookIndex(event, id)`: index of `id` in the order for `event`; `None` where the JS returns -1.
pub fn hook_index(event: &str, id: &str) -> Option<usize> {
    HookPoint::from_name(event)?.order().iter().position(|h| *h == id)
}
