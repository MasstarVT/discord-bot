// Old dashboard bookmarks lived at "/#/guild/..."; the dashboard now sits at /dashboard, so forward them there.
// Guarded so the file stays import-safe in Node (test/smoke.test.js imports every source file).
if (typeof location !== 'undefined' && location.hash.startsWith('#/')) location.replace(`/dashboard${location.hash}`);
