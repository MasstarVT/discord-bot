//! Port of `src/services/reactionRoleService.js` (pkg-reaction-roles): the pure helpers (emoji and link parsing, the
//! role-change planner, payload builders), the per-guild panel index used by the reaction hooks, the DB access used
//! by `/reactionrole` and the dashboard, the Discord operations (sync, reactions, role changes), the persistent
//! button/select handlers and the hook bodies. Every function the JS exports is re-exported here.

pub mod discord;
pub mod emoji_tables;
pub mod engine;
pub mod handlers;
pub mod index;
pub mod pure;
pub mod store;

pub use discord::*;
pub use engine::*;
pub use handlers::*;
pub use index::*;
pub use pure::*;
pub use store::*;
