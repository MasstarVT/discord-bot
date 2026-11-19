//! Port of `src/plugins/games-fixes.js`: the plugin declaration of the games package. It registers the persistent
//! Scrabble rack button at startup, so a board posted before a restart still answers ("no longer active") instead of
//! failing. No init: the dictionary is loaded lazily by /scrabble. The collector prefixes (`ttt`, `ttt-ch`, `rps`,
//! `scrabble-ch`) are not registered here; the router answers an unhandled click on them with "This menu has
//! expired." after its grace period.

use bot_core::Plugin;
use bot_core::prelude::ModuleKey;

use crate::scrabble_service::handle_rack_button;

/// The `games-fixes` plugin (module `games`: the wrapper answers "The Games module is turned off…" when it is
/// disabled).
pub fn plugin() -> Plugin {
    Plugin::new("games-fixes", Some(ModuleKey::Games)).button("scrabble-rack", |ctx| async move { handle_rack_button(ctx).await })
}
