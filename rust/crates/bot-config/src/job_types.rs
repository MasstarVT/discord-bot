//! Port of `src/config/jobTypes.js`: the scheduler job registry.

pub use crate::tables::{INTERACTIVE_JOB_TYPES, JOB_TYPES, JobType, RECURRING_JOB_TYPES};

crate::string_enum_impls!(JobType);

/// One `JOB_TYPES` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobTypeInfo {
    pub job_type: JobType,
    pub owner: &'static str,
    /// Re-seeds itself via `{ nextRunAt }`; cancelled when the bot leaves a guild.
    pub recurring: bool,
    /// Documents the `dedupeKey` shape, e.g. `mod.unban:{guildId}:{userId}`.
    pub dedupe: &'static str,
    /// `interactive: true` (absent in the JS means false).
    pub interactive: bool,
}

impl JobType {
    /// `JOB_TYPES[type]`.
    pub fn info(self) -> &'static JobTypeInfo {
        // JOB_TYPES is generated in the same order as the enum (checked by the tests).
        &JOB_TYPES[self as usize]
    }

    pub fn is_recurring(self) -> bool {
        self.info().recurring
    }

    pub fn is_interactive(self) -> bool {
        self.info().interactive
    }
}

/// `isJobType(type)`.
pub fn is_job_type(job_type: &str) -> bool {
    JobType::from_name(job_type).is_some()
}
