//! bot-cards: the PNG cards the bot draws, ported from `src/utils/welcomeCard.js` (700x250 welcome card) and
//! `src/utils/profileCard.js` (900x280 rank card). Pure Rust: tiny-skia draws, rustybuzz shapes the embedded
//! Nunito-Bold font (the deployed image has no system fonts), the `image` crate decodes avatars and backgrounds.
//!
//! The drawing functions ([`render_welcome_card`], [`generate_profile_card`]) take plain data and image bytes and do
//! no I/O. Network access stays in the loaders ([`welcome_card::load_avatar`], [`welcome_card::load_card_background`],
//! [`profile_card::load_card_background`] and the two `validate_card_background`), which go through
//! `bot_util::safe_fetch` and keep the JS caches.

pub mod canvas;
pub mod font;
pub mod profile_card;
pub mod welcome_card;

pub use canvas::CardError;
pub use profile_card::{
    ProfileCardOptions, RankCardStyle, default_rank_card_style, generate_profile_card,
    normalize_card_style,
};
pub use welcome_card::{LEGACY_CARD_COLORS, WelcomeCardData, WelcomeCardStyle, generate_welcome_card, render_welcome_card};
