//! `node index.js` (`npm start`) and `node index.js --deploy` (`npm run deploy`).
//!
//! Usage: `bot` starts the bot; `bot deploy` or `bot --deploy` pushes the slash commands and exits.
//! Run it from the repository root: `.env`, `state/` and `.deploy-hash` are read there, as the JS bot does.

fn main() {
    // `import 'dotenv/config'`: first, while this is the only thread.
    bot::dotenv::load_into_process_env();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let deploy = args.iter().any(|a| a == "--deploy") || args.first().is_some_and(|a| a == "deploy");
    let env = bot::index::Env::from_process();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!("Fatal error during startup", bot_util::logger::LogArg::error(&e));
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(async move {
        if deploy {
            bot::index::deploy_mode(&env.deploy_config()).await
        } else {
            bot::index::run(env).await
        }
    });
    std::process::exit(code);
}
