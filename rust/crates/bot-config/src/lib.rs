//! bot-config: the registries and constants of `src/config/*.js`.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/config/constants.js` | [`constants`] |
//! | `src/config/customIds.js` | [`custom_ids`] |
//! | `src/config/hookPoints.js` | [`hook_points`] |
//! | `src/config/jobTypes.js` | [`job_types`] |
//! | `src/config/modules.js` | [`modules`] |
//!
//! The large tables live in [`tables`], generated from the JS by `rust/fixtures/gen/bot-config-tables.mjs`.
//! Every string-valued enum converts to and from exactly the JS string (`as_str`, `from_name`, `Display`,
//! `FromStr`, serde as a plain string).

pub mod constants;
pub mod custom_ids;
pub mod hook_points;
pub mod job_types;
pub mod modules;
pub mod tables;

pub use constants::*;
pub use custom_ids::*;
pub use hook_points::*;
pub use job_types::*;
pub use modules::*;

/// Error from `FromStr` on a registry enum: the string is not a registered name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownName {
    /// Which registry was searched (`"ModuleKey"`, `"JobType"`, ...).
    pub kind: &'static str,
    /// The string that matched nothing.
    pub value: String,
}

impl std::fmt::Display for UnknownName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown {} \"{}\"", self.kind, self.value)
    }
}

impl std::error::Error for UnknownName {}

/// Display, FromStr, AsRef<str> and string serde for an enum with `as_str` / `from_name`.
macro_rules! string_enum_impls {
    ($($ty:ident),* $(,)?) => {$(
        impl ::std::fmt::Display for $ty {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl ::std::str::FromStr for $ty {
            type Err = $crate::UnknownName;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $ty::from_name(s).ok_or_else(|| $crate::UnknownName { kind: stringify!($ty), value: s.to_owned() })
            }
        }
        impl AsRef<str> for $ty {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
        impl ::serde::Serialize for $ty {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <::std::borrow::Cow<'de, str> as ::serde::Deserialize>::deserialize(d)?;
                $ty::from_name(&s).ok_or_else(|| ::serde::de::Error::custom(format!("unknown {} \"{}\"", stringify!($ty), s)))
            }
        }
    )*};
}
pub(crate) use string_enum_impls;
