//! Port of `src/utils/calendar.js`: "Add to calendar" links (Google Calendar, Outlook.com) and an iCalendar file.
//!
//! The pure implementation already lives in `bot_util::calendar` (the foundation ported it with the other utils); this
//! module re-exports it under the events crate and adds the JS default arguments, so callers read like the JS.

pub use bot_util::calendar::{
    CalendarEntry, DEFAULT_PROD_ID, LINK_URL_MAX, build_ics, fit_encoded, google_calendar_url, ics_escape, ics_fold, outlook_calendar_url,
};
use chrono::{DateTime, Utc};

/// `googleCalendarUrl(entry)` with the default cap of [`LINK_URL_MAX`].
pub fn google_url(entry: &CalendarEntry) -> String {
    google_calendar_url(entry, LINK_URL_MAX)
}

/// `outlookCalendarUrl(entry)` with the default cap of [`LINK_URL_MAX`].
pub fn outlook_url(entry: &CalendarEntry) -> String {
    outlook_calendar_url(entry, LINK_URL_MAX)
}

/// `buildIcs(entry, { now })` with the default PRODID.
pub fn ics(entry: &CalendarEntry, now: DateTime<Utc>) -> String {
    build_ics(entry, now, DEFAULT_PROD_ID)
}
