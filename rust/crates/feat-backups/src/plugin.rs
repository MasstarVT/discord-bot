//! Port of `src/plugins/backups.js`: server backups (roles, categories, channels and permission overwrites) and the
//! restore that rebuilds what is missing. Declaration only: every body lives in [`crate::backup_service`].

use bot_core::prelude::*;

use crate::backup_service;

/// The `backups` plugin.
pub fn plugin() -> Plugin {
    Plugin::new("backups", Some(ModuleKey::Backups))
        // Database only: keeps the recurring backup.auto job alive after a restart (the job reads the settings itself, so
        // the module being off at that moment does not matter). `always` because the module may be turned on later.
        .hook(Hook::<GuildReady>::new("backups", backup_service::on_guild_ready).always())
        // module: null: the confirmation works while the module is off. The handler checks for the SERVER OWNER itself.
        .button("backup", backup_service::handle_backup_button)
        .component_module(ModuleRef::Ungated)
        // Not gated by the runner: the handlers read the module toggle themselves, and a running restore always finishes.
        .job(JobType::BackupAuto, job_handler(backup_service::handle_auto_job))
        .job(JobType::BackupRestore, job_handler(backup_service::handle_restore_job))
}
