# dash-auth: public API (from its author)

I rebuilt the crate from its skeleton after the crash. Files ported, JS to Rust:
- src/dashboard/config.js → rust/crates/dash-auth/src/config.rs, with Node's net.isIP in src/js_ip.rs
- src/dashboard/auth.js → src/auth.rs
- src/dashboard/siteParts.js → src/site_parts.rs
- src/dashboard/docs.js → src/docs.rs
- src/dashboard/changelog.js → src/changelog.rs
- src/dashboard/commandList.js → src/command_list.rs
- The pure rate-limit parts of src/dashboard/server.js → src/rate_limit.rs. auth.js has no rate limits; they live in server.js. This module holds createRateLimiter, clientAddress, loginSourceKey, the five limit constants, LOOPBACK_ADDRESSES, and the login callback's per-source / overall / first-attempt allowance with its unresolved marks, as LoginLimiter.

The session cookie format is unchanged: HKDF-SHA256 key (salt discord-bot-dashboard, info session-cookie/v1), AES-256-GCM with a random 12-byte IV and AAD "v1", written as `v1.` plus base64url of iv|ciphertext|tag. Decoding is Node-lenient base64url, the checks run in the JS order, and the size limit is 4096.

Goldens come from rust/fixtures/gen/dash-auth-golden.mjs and are written to rust/fixtures/golden/dash-auth/{config,ip,rate-limiter,auth,html,command-list}.json. They cover:
- 70 config environments, valid and invalid, compared byte for byte
- isIP, normalizeIp, loginSourceKey and clientAddress tables
- derived keys, JS-sealed cookies and cookies that must not open
- cookie parsing and serialising
- authorize URLs and the exact token and revoke requests (URL, Basic header, body)
- about 40 token-endpoint answers
- canManageGuild and checkWriteRequest tables
- site-part HTML; docs from the real catalog and commands.json plus an odd synthetic catalog; changelog from the real CHANGELOG.md plus odd markdown
- summarizeCommands over all 79 commands and an ICU sort table

rust/fixtures/gen/dash-auth-seal.mjs runs live in the tests in both directions. Cookies sealed by Rust open in the JS, including after parseCookies of the Set-Cookie value. Cookies sealed by the JS open in Rust. Both sides refuse a cookie at its exp.

The auth, config and site cases of test/dashboard.test.js are ported, as are the clientAddress and login rate-limit cases of test/review-dashboard-server.test.js and the login-allowance and loginSourceKey cases of test/sec-dashboard.test.js. The crate has no SQL or cache code, so no database tests exist. The suite was still run with TEST_DATABASE_URL and TEST_REDIS_URL set (db dash_auth). No test needs the network; one test points the reqwest client at a dead local port.

## API

The crate is dash_auth. Every time-dependent call takes now_ms as an f64 (ms since epoch). Nothing starts a server.

config:
- load_dashboard_config(|key| Option<String>), plus load_dashboard_config_from_env() and load_dashboard_config_from_map(&HashMap). They return LoadedConfig { ok, missing, invalid: Vec<InvalidKey{key, reason}>, warnings, config: DashboardConfig }.
- DashboardConfig fields: client_id, bot_token, client_secret, session_secret, database_url, redis_url, bot_owner_id, url, origin, url_is_default, redirect_uri, secure_cookies, port: u16, host, trusted_proxies: Vec<String>, contact: Option<Contact{kind: ContactKind::Email|Url, value}>, support_url. It serialises to the JS object's JSON.
- Also: parse_dashboard_url(raw) -> Result<origin, reason>, parse_contact, normalize_ip, is_snowflake, REQUIRED_KEYS, MIN_SESSION_SECRET_LENGTH, DEFAULT_URL / DEFAULT_PORT / DEFAULT_HOST.
- js_ip::is_ip / is_ipv4 / is_ipv6 are Node's net.isIP.

auth constants: SESSION_COOKIE, STATE_COOKIE, SESSION_MAX_AGE_SECONDS, STATE_MAX_AGE_SECONDS, OAUTH_SCOPES, OAUTH_TIMEOUT_MS, TOKEN_URL, TOKEN_REVOCATION_URL, AUTHORIZE_URL.

auth functions:
- derive_session_key(secret) -> Result<SessionKey, &str>
- seal_session(&Value, &SessionKey) -> String
- unseal_session(Option<&str>, &SessionKey, now_ms) -> Option<Map<String, Value>>
- parse_cookies(Option<&str>) -> IndexMap (first value wins); decode_uri_component
- serialize_cookie(name, value, &CookieOptions{max_age: Option<f64>, path, secure}) and clear_cookie(name, path, secure)
- create_oauth_state(), states_match(Option<&str>, Option<&str>), build_authorize_url(client_id, redirect_uri, state)
- OAuthCredentials, built from &DashboardConfig
- OAuthClient::new() or with_endpoints(token_url, revoke_url): no redirects, 10 s timeout. Methods: .exchange_code(&creds, code) -> Result<TokenGrant{access_token, expires_in: f64}, OAuthError{message, status: Option<u16>}> and .revoke_token(&creds, token) -> bool.
- Transport-agnostic versions: exchange_code_with and revoke_token_with, plus token_request, revoke_request, parse_token_response, basic_auth.
- can_manage_guild(Option<&Value>) takes the raw OAuth guild entry.
- check_write_request(method, &WriteHeaders{content_type, x_dashboard, origin, sec_fetch_site}, config_origin) -> Result<(), WriteRejection{status, error, message}>. Build WriteHeaders with Node semantics: first Content-Type only, other duplicates joined with ', '.
- is_http_origin(value).

rate_limit:
- RateLimiter::new(RateLimit{max, window_ms}).hit(key, now_ms) -> RateDecision::Ok | Limited{retry_after}
- Constants: WRITE_RATE_LIMIT, SEARCH_RATE_LIMIT, LOGIN_RATE_LIMIT, LOGIN_GLOBAL_RATE_LIMIT, LOGIN_FIRST_ATTEMPT_RATE_LIMIT, RATE_LIMIT_KEYS_MAX, LOOPBACK_ADDRESSES.
- client_address(peer: Option<&str>, x_forwarded_for: Option<&str>, trusted: &HashSet<normalised ip>) and login_source_key(address).
- LoginLimiter::new(). Call .check(source, now_ms) once a callback has a matching state and a well-formed code, before the token exchange. On Limited, answer 429 'Too many login attempts. Try again in {n} s.' with Retry-After; LoginCheck.overall tells the log line which limit refused it. Call .resolved(source) after a successful login.

site_parts:
- escape_html, flavour_of / FLAVOURS, who_label, public_description, public_note
- render_contact(Option<&Contact>), render_support(Option<&str>, label) with SUPPORT_LABEL
- inline_markdown, format_default(&Value field), scalar_settings(&Value module), REPO_URL, ISSUES_URL

docs:
- module_id, command_id
- render_docs_toc(&catalog_json), render_docs_modules(&catalog_json, &[CommandSummary]), render_core_commands(&[CommandSummary])
- The catalog is the serde_json::Value of buildCatalog() in its /api/catalog JSON form.

changelog: parse_changelog(&str) -> Changelog{intro, entries: Vec<ChangelogEntry{date, title, items}>}, entry_id, render_latest_changes(entries, count) with LATEST_COUNT = 3, render_changelog_entries.

command_list:
- CommandSummary{name, description, module, who, subcommands} deserialises src/dashboard/commands.json directly.
- summarize_commands(&[CommandEntry{module, permission_level, payload: &Value}]), subcommands(payload), who_of(level), command_name_compare.

## Known gaps

- auth.rs OAuthClient::send (auth.js postForm with redirect: 'error'): a 3xx answer becomes the error 'fetch failed' with no status, as real fetch throws. The pure parse_token_response says 'token exchange returned HTTP 302', as the JS does with a fake 302 Response. Timeouts give 'The operation was aborted due to timeout' and other transport errors give 'fetch failed'; both were checked against Node 24. Other undici-specific error texts are not reproduced.
- command_list.rs command_name_compare (commandList.js summarizeCommands, localeCompare): the ICU root order is reproduced for ASCII letters, digits, '_' and '-', with case as the tertiary level and NFD accents as the secondary level. It was checked against a Node-sorted table and the real command list. Other scripts and punctuation fall back to code-point order, so it is not full ICU.
- config.rs load_dashboard_config_from_env (config.js loadDashboardConfig(process.env)): an environment value that is not valid Unicode counts as unset. Node would read it as a lossily decoded string.
- site_parts.rs render_contact (siteParts.js renderContact): takes the typed config::Contact. JS objects with an unknown kind or a non-string value have no Rust form; both render '' in the JS, as the golden checks.
- auth.rs check_write_request (auth.js checkWriteRequest): takes a WriteHeaders struct instead of the Node request. The caller must apply Node's header semantics: only the first Content-Type is kept, and other repeated headers are joined with ', '.
- auth.rs unseal_session: JSON.parse is serde_json. Numbers out of f64 range are a parse error, and the result is the same null. A payload whose exp is a float is written by serde_json as e.g. 1.5 or 1800000000000.0 when sealed from an f64 Value, so Rust callers should write exp as an integer. The JS opens either form.
- rate_limit.rs: these helpers are ported from server.js, which the dash-server agent owns, so dash-server can wire them in instead of porting them again. contactHtml, isValidSession and the HTTP flow itself remain dash-server's.
