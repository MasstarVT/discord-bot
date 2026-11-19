//! `node dashboard.js` (`npm run dashboard`): the web dashboard, a separate process from the bot.
//! Run it from the repository root, where `.env` is read, as the JS dashboard does.

fn main() {
    // `import 'dotenv/config'`: first, while this is the only thread.
    bot::dotenv::load_into_process_env();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!("Fatal error during startup", bot_util::logger::LogArg::error(&e));
            std::process::exit(1);
        }
    };
    let code = runtime.block_on(dashboard::index::run());
    std::process::exit(code);
}
