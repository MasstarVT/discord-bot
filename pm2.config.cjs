module.exports = {
  apps: [
    {
      name: 'discord-bot',
      script: 'index.js',
      interpreter: 'node',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '512M',
      // index.js gives the shards 25 s to shut down gracefully; PM2 must not SIGKILL earlier (R201).
      kill_timeout: 26000,
      error_file: './logs/pm2-error.log',
      out_file: './logs/pm2-out.log',
      log_date_format: 'YYYY-MM-DD HH:mm:ss Z',
      merge_logs: true,
      env_development: {
        NODE_ENV: 'development',
      },
      env_production: {
        NODE_ENV: 'production',
      },
    },
    {
      // The web dashboard (dashboard.js): a separate process sharing the bot's database and Redis.
      // Opt-in: `npm run pm2:dashboard` starts it; `npm run pm2:start` starts only the bot.
      name: 'discord-bot-dashboard',
      script: 'dashboard.js',
      interpreter: 'node',
      instances: 1,
      autorestart: true,
      watch: false,
      max_memory_restart: '256M',
      // dashboard.js exits within 10 s of SIGTERM.
      kill_timeout: 11000,
      error_file: './logs/pm2-dashboard-error.log',
      out_file: './logs/pm2-dashboard-out.log',
      log_date_format: 'YYYY-MM-DD HH:mm:ss Z',
      merge_logs: true,
      env_development: {
        NODE_ENV: 'development',
      },
      env_production: {
        NODE_ENV: 'production',
      },
    },
  ],
};
