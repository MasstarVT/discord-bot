//! Port of `src/services/automationService.js`: Automations (When -> If -> Do).
//!
//! The JS file is split by concern; every export is re-exported here, so callers use
//! `feat_automations::automation_service::*` as they would the JS module:
//!
//! | Part | What |
//! |---|---|
//! | [`tables`] | constants, the trigger / condition / action tables, error texts, cache keys, `RAW_SQL` |
//! | [`pure`] | normalisers, shape validation, the evaluator, loop protection, presets, JSON import / export, descriptions |
//! | [`store`] | the per-guild memo and Redis copy, reads, the row-locked writes |
//! | [`options`] | the `/automation` option mapping, configuration-time checks, import references, the problem list |
//! | [`runtime`] | facts, the action executor, the dispatcher, the hook bodies and the button handler |

pub mod options;
pub mod pure;
pub mod runtime;
pub mod store;
pub mod tables;

pub use options::*;
pub use pure::*;
pub use runtime::*;
pub use store::*;
pub use tables::*;
