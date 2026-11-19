//! bot-util: the pure utilities of `src/utils` (no Discord library types). Each module mirrors one JS file:
//!
//! | JS (`src/utils/`) | Rust |
//! |---|---|
//! | async.js | [`async_util`] (file `async.rs`) |
//! | calendar.js | [`calendar`] |
//! | errorFormatter.js | [`error_formatter`] |
//! | fatalGateway.js | [`fatal_gateway`] |
//! | gatewayFeatures.js | [`gateway_features`] |
//! | healthServer.js | [`health_server`] |
//! | imageSize.js | [`image_size`] |
//! | logger.js | [`logger`] (macros `log_info!`, `log_warn!`, `log_error!`, `log_debug!`, `log_success!`) |
//! | parseDuration.js | [`parse_duration`] |
//! | placeholderParser.js | [`placeholder_parser`] |
//! | restartBrake.js | [`restart_brake`] |
//! | safeFetch.js | [`safe_fetch`] |
//! | safeRegex.js | [`safe_regex`] |
//! | text.js | [`text`] |
//! | time.js | [`time`] |
//! | timeMentions.js | [`time_mentions`] |
//! | timezones.js | [`timezones`] |
//!
//! [`js`] and [`jsre`] hold the JavaScript semantics the ports rely on (UTF-16 lengths, number formatting, Date
//! maths, ECMAScript regexes); `tzdata` holds the ICU zone data generated from Node.

pub mod js;
pub mod jsre;
mod tzdata;
mod v8_regexp;

#[path = "async.rs"]
pub mod async_util;
pub mod calendar;
pub mod error_formatter;
pub mod fatal_gateway;
pub mod gateway_features;
pub mod health_server;
pub mod image_size;
pub mod logger;
pub mod parse_duration;
pub mod placeholder_parser;
pub mod restart_brake;
pub mod safe_fetch;
pub mod safe_regex;
pub mod text;
pub mod time;
pub mod time_mentions;
pub mod timezones;
pub mod v8_date;

pub use tzdata::{icu_tzdata_version, locale_compare};
