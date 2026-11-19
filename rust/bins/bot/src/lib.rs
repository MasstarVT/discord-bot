//! The Discord bot binary as a library, so the tests can drive each start-up step.
//!
//! | JS | Rust |
//! |---|---|
//! | `import 'dotenv/config'` | [`dotenv`] |
//! | `index.js` (health endpoints, signals, restart brake, auto-deploy, shard start, parks, `--deploy`) | [`index`] |
//! | `src/bot.js` (the client, `client.shutdown()`) | [`bot`] |
//! | the loaders' file discovery (`src/commands/**`, `src/plugins/*.js`) | [`registry`] |
//!
//! `src/handlers/*` and `src/events/*` are ported in bot-core (`registry`, `plugin`, `router`, `command`, `gateway`);
//! `src/utils/{healthServer,restartBrake,fatalGateway,gatewayFeatures}.js` in bot-util.

pub mod bot;
pub mod dotenv;
pub mod index;
pub mod registry;
