//! Port of `src/plugins/birthdays.js`: the guildReady self-healing pass and the `birthday.tick` job handler. The logic
//! lives in [`crate::birthday_service`].

use bot_core::prelude::*;

use crate::birthday_service::{on_guild_ready, run_birthday_tick};

/// The `birthdays` plugin.
pub fn plugin() -> Plugin {
    Plugin::new("birthdays", Some(ModuleKey::Birthdays))
        // always: the tick keeps running while the module is off. It removes the birthday role, and it keeps recurring
        // so that `/module enable birthdays` takes effect within an hour.
        .hook(
            Hook::<GuildReady>::new("birthdays", |ctx| async move {
                let settings = ctx.get_settings().await;
                on_guild_ready(&ctx.app, &ctx.data.guild_id.to_string(), settings.as_deref()).await;
                Ok(())
            })
            .always(),
        )
        .job(JobType::BirthdayTick, job_handler(|job, ctx| async move { Ok(run_birthday_tick(&ctx.app, &job.guild_id).await) }))
}
