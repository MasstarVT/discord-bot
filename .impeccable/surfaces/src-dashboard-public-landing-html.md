---
version: 1
slug: "src-dashboard-public-landing-html"
primary_target: "src/dashboard/public/landing.html"
related_targets: ["src/dashboard/public/docs.html","src/dashboard/public/changelog.html"]
---

# Surface brief: public site (landing, /docs, /changelog)

Scope: `src/dashboard/public/landing.html` (Persuade), `docs.html` and `changelog.html` (Read, same world), the favicon. The dashboard app (`index.html`, `styles.css`, `tokens.css`) keeps its own look.

Audience: Discord server owners/admins deciding whether to invite Frost.Byte. Action: Add to Discord. Proof: a live command demo in the carton window, real rank/welcome cards from the bot's renderer (sample data, labeled), every module unwrappable to its real commands and settings, real counts (28 modules, 81 commands), dated changelog, docs, GitHub, support server when configured.

Unresolved: the support server invite (env `DASHBOARD_SUPPORT_URL`, owner to set).

## Direction contract

THESIS: Frost.Byte is sold as a freezer-aisle variety pack: one carton, 28 individually wrapped modules, pick the flavours your server wants. Refuses the dark SaaS split hero with a Discord mock and a feature-card grid.

OWN-WORLD: Printed carton board. Freezer-blue carton ink owns the page, frost-white inner board for info panels, navy print ink, four flavour inks (cherry, lemon, lime, grape), one per catalog category, always printed with the module's two-letter code. Crimped wrapper ends, dashed fold lines, a die-cut window with flat cellophane glints, a starburst price sticker, a barcode. Flat ink only: no gradients, glow or shadows. Display: Bricolage Grotesque; panel copy: Archivo (width axis for stamps); commands: Spline Sans Mono.

STORY: The visitor sees the box, reads "28 modules, one bot, free", watches a command run in the window, unwraps a module or two to see its real commands and settings, reads the Bot Facts and directions, and adds the bot.

FIRST VIEWPORT: Full-bleed carton blue front panel. Left: snowflake seal and a huge Frost.Byte lockup, the one-line offer, the lemon starburst "Add to Discord" sticker as the primary action with "Log in" beside it. Right: the die-cut window (about 45% width) holding the live Discord chat demo with flavour command buttons under it. Bottom edge: "NET 81 slash commands" mark and a fold line into the next panel.

FORM: Variety Pack (freezer-aisle ice-pop carton), grounded candidate 7 of 7, seed key daabfdfc. The page is the carton's dieline unfolded top to bottom: front panel, side panel (what's inside), back panel (Bot Facts, directions, serving suggestion, ingredients), bottom flap (lot dates and links). Signature interaction: unwrapping a module wrapper (a `<details>` that tears open along its crimp) to show its ingredients. Motion: the tear, once, eased out; demo messages arrive with a short settle; reduced motion removes both. Raises: identical wrapper units as the ruler (VU bridge); one command per step with before/after state (forge); colour always labeled with the code (silk canopy); total flat-ink commitment per panel (OSPAAAL).

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
