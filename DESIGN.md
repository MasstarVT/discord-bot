---
name: Frost.Byte public site
description: A free Discord bot sold as a freezer-aisle variety pack of 28 individually wrapped modules.
colors:
  carton: "#1B4FCF"
  carton-deep: "#143FAE"
  carton-line: "#6E95EA"
  on-carton: "#FFFFFF"
  on-carton-soft: "#D6E3FF"
  board: "#EEF4FF"
  board-raised: "#FFFFFF"
  board-line: "#C3D3EF"
  ink: "#0B1A3F"
  ink-soft: "#33446E"
  ink-line: "#8FA3CC"
  cherry: "#D8213C"
  on-cherry: "#FFFFFF"
  lemon: "#FFD23A"
  on-lemon: "#0B1A3F"
  lime: "#4CC254"
  on-lime: "#0B1A3F"
  grape: "#6E3BD8"
  on-grape: "#FFFFFF"
typography:
  display:
    fontFamily: "Bricolage Grotesque, Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(60px, 22vw, 136px)"
    fontWeight: 800
    lineHeight: 0.86
    letterSpacing: "-0.022em"
    fontVariation: "\"opsz\" 96, \"wdth\" 80"
  headline:
    fontFamily: "Bricolage Grotesque, Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(34px, 5vw, 60px)"
    fontWeight: 800
    lineHeight: 1
    letterSpacing: "-0.03em"
    fontVariation: "\"opsz\" 96"
  title:
    fontFamily: "Bricolage Grotesque, Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "clamp(24px, 3vw, 32px)"
    fontWeight: 800
    lineHeight: 1.1
    letterSpacing: "-0.02em"
  body:
    fontFamily: "Archivo, ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.55
    fontVariation: "\"wdth\" 100"
  label:
    fontFamily: "Archivo, ui-sans-serif, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 800
    letterSpacing: "0.06em"
    fontVariation: "\"wdth\" 125"
  mono:
    fontFamily: "Spline Sans Mono, ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "13px"
    fontWeight: 600
rounded:
  chip: "5px"
  panel: "14px"
  pill: "999px"
spacing:
  gutter: "16px"
  gutter-wide: "24px"
  panel-y: "64px"
  panel-y-wide: "96px"
  wrap: "1240px"
components:
  tab-lemon:
    backgroundColor: "{colors.lemon}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "10px 18px"
    height: "44px"
  tab-lemon-hover:
    backgroundColor: "{colors.board-raised}"
    textColor: "{colors.ink}"
  tab-line:
    backgroundColor: "transparent"
    textColor: "{colors.on-carton}"
    rounded: "{rounded.pill}"
    padding: "10px 18px"
    height: "44px"
  tab-line-hover:
    backgroundColor: "{colors.on-carton}"
    textColor: "{colors.carton}"
  tab-ink:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.board-raised}"
    rounded: "{rounded.pill}"
    padding: "10px 18px"
    height: "44px"
  tab-ink-hover:
    backgroundColor: "{colors.carton}"
    textColor: "{colors.board-raised}"
  sticker:
    backgroundColor: "{colors.lemon}"
    textColor: "{colors.ink}"
    size: "176px"
  sticker-hover:
    backgroundColor: "{colors.board-raised}"
  pop-wrapper:
    textColor: "{colors.on-cherry}"
    backgroundColor: "{colors.cherry}"
    padding: "12px 22px"
    height: "92px"
  pop-inside:
    backgroundColor: "{colors.board-raised}"
    textColor: "{colors.ink}"
    padding: "16px 18px 18px"
  taste:
    backgroundColor: "{colors.lime}"
    textColor: "{colors.on-lime}"
    typography: "{typography.mono}"
    padding: "5px 14px 5px 8px"
    height: "40px"
  input:
    backgroundColor: "{colors.board}"
    textColor: "{colors.ink}"
    typography: "{typography.mono}"
    rounded: "8px"
    padding: "8px 10px"
  segment:
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "6px 12px"
  segment-selected:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.board-raised}"
  panel-board:
    backgroundColor: "{colors.board-raised}"
    textColor: "{colors.ink}"
    rounded: "{rounded.panel}"
    padding: "18px"
  bot-facts:
    backgroundColor: "{colors.board-raised}"
    textColor: "{colors.ink}"
    padding: "10px 14px 14px"
    width: "380px"
  bottom-flap:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.on-carton-soft}"
    padding: "56px 0 40px"
---

# Design System: Frost.Byte public site

## Overview

**Creative North Star: "The Variety Pack"**

The public site is a printed freezer-aisle carton of ice pops, unfolded top to bottom along its dieline. Freezer-blue carton board owns the page. Frost-white inner board carries the reading panels, navy print ink sets the copy, and four flavour inks (cherry, lemon, lime, grape) mark the catalog's four categories. Each module is one individually wrapped pop with crimped, heat-sealed ends. The carton's furniture is real: dashed score and fold lines, a die-cut window with flat cellophane glints, a starburst price sticker, a nutrition-style Bot Facts panel, lot-dated "Packed on" stamps, a "Tear here to open" strip and a barcode.

Everything is flat print. Depth comes from ink on board and from cut edges (the window's thick frost rim, the wrapper's crimp), never from light. The density is packaging density: big confident display type on the panels, and small wide capitals ("stamps") for the labels a carton actually prints. Motion is a few physical events: the wrapper end tears when a module unwraps, demo messages settle into the window, and the sticker tilts toward you on hover. The direction contract placed a snowflake seal beside the front lockup; the build carries the mark only in the top flap, the demo avatar and the close, and the build is the record.

Scope: this world covers only the public pages (`/`, `/docs`, `/changelog`, and the legal pages `/privacy` and `/terms`) and the favicon. Their files are `site.css` (tokens and shared chrome), `landing.css`, `leaflet.css`, `landing.html`, `docs.html`, `changelog.html`, `privacy.html`, `terms.html`, `landing.js` and `favicon.svg`, plus the server renderers `landing.js`, `docs.js`, `changelog.js` and `siteParts.js`. The legal pages are leaflet pages: plain paragraphs and lists under `.leaf-legal`, with a `.leaf-date` stamp for the last-updated date. The dashboard app (`index.html`, `styles.css`, `tokens.css`, `forms.css`, `collections.css`) keeps the older "Polar Night" look and is not governed by this file.

**Key Characteristics:**
- Freezer-blue carton ground with frost-white board panels and navy print ink.
- Four flavour inks, one per catalog category, each with a fixed ink printed on it.
- Crimped wrapper units of identical size act as the page's ruler.
- Dashed fold lines between panels and sections; solid rules inside them.
- Flat ink only: no shadows, gradients, glows or blurs.
- Bricolage Grotesque display, Archivo copy and stamps (width axis), Spline Sans Mono commands.

## Colors

A printed carton palette: one saturated board blue, a cool white inner board, a near-black navy ink, and four candy-bright flavour inks.

### Primary
- **Freezer Carton Blue** (carton): the page ground. Front panel, back panel, the top flap, the favicon's hexagon and the bot's avatar in demos. White print sits on it.
- **Pressed Carton** (carton-deep): the board edge. Inline commands printed on the carton, the scrollbar thumb and the registration bar's first swatch.
- **Carton Fold Blue** (carton-line): dashed score and fold lines on the carton, the smallprint rule and the lot-date outlines.

### Secondary
- **Lemon Sticker** (lemon): community-category flavour, and also the system's action and highlight ink. It fills the starburst sticker, the primary tab, text selection, the focus ring on carton, the directions' step discs, `:target` highlights in docs and changelog, and the stamp colour on the dark panels.

### Tertiary
- **Cherry** (cherry, white print): the safety category (moderation, automod, logs).
- **Lime** (lime, navy print): the engagement category. It also fills the Levels tile in the serving suggestion.
- **Grape** (grape, white print): the tools category.

### Neutral
- **Frost Board** (board): the inner-board panels (What's inside, the close, the docs and changelog leaflet), input fills and code chips on raised board.
- **Raised Board** (board-raised): unwrapped pop contents, the sample dashboard form, Bot Facts, the docs contents box and the barcode plate.
- **Board Rule** (board-line): hairline dividers inside board panels: settings rows, command rows, module separators.
- **Print Navy** (ink): body copy on board, the bottom flap's ground, heavy rules (flavour heads, Facts bars, table heads) and the ink tab.
- **Faded Ink** (ink-soft): secondary copy on board, captions and stamp labels on board.
- **Ink Rule** (ink-line): dashed fold lines on board, the input stroke and code-chip outlines.
- **Frost Print** (on-carton-soft): secondary copy on the carton and the bottom flap.

The Discord channel inside the die-cut window uses Discord's own dark-theme greys, blurple and embed stripes. That is the product seen through the cellophane, scoped to the window, and is not part of this palette.

### Named Rules
**The Labeled Flavour Rule.** A flavour ink always travels with its label. On a module it is printed with the module's two-letter code (the wrapper disc, the taste chip, the docs module disc). On a category it sits next to the flavour or category name (the category pill, the docs category title, the contents dot). The one exemption is the six-swatch registration bar in the front smallprint, a printer's mark that is decorative and hidden from assistive tech.

**The Fixed Print Rule.** Each flavour has one ink printed on it and never another: white on cherry and grape, navy on lemon and lime. Components pick up the pair through `--flav` and `--on-flav`.

**The Lemon Means Go Rule.** Outside its category role, lemon is reserved for the primary action and for "you are here" highlights: the sticker, the lemon tab, selection, the focus ring on carton, and targeted lots and commands.

## Typography

**Display Font:** Bricolage Grotesque (with Archivo, system sans fallback)
**Body Font:** Archivo (with system sans fallback)
**Label/Mono Font:** Archivo at width 125 for stamps; Spline Sans Mono for commands, values and lot dates

**Character:** A chunky, slightly quirky grotesque with optical-size and width axes for the carton's lettering, over a sturdy workhorse sans that widens into packaging capitals. Commands are set in a soft monospace so they read as things you type.

### Hierarchy
- **Display** (800, clamp(60px, 22vw, 136px), 0.86, opsz 96 / wdth 80): the Frost.Byte lockup on the front panel only.
- **Headline** (800, clamp(34px, 5vw, 60px), 1.0): panel titles. The leaflet title (clamp(46px, 7.5vw, 100px), wdth 82) and the close title (clamp(40px, 7vw, 88px)) are larger siblings of this role.
- **Title** (800, clamp(24px, 3vw, 32px), 1.1): category heads, docs section heads (up to 44px), changelog lot names and module names (20 to 27px).
- **Body** (400, 17px, 1.55): all running copy. Leads step up to 18px. Measure is held at 44 to 72ch.
- **Label** (800, 12px, 0.06em, uppercase, wdth 125): the stamp, used for packaging labels. Variants run 11 to 12.5px.
- **Mono** (600, 13 to 16px): command names in taste chips, setting values, input values, lot dates and stamps, barcode digits.

The Bot Facts title is the one deliberate exception: Archivo 900 at width 88, 46px, quoting the nutrition-label convention.

### Named Rules
**The Stamp Labels Things Rule.** Stamps name a field, group or label printed on the pack ("Net", "Ingredients", "Packed on", "Tear here to open", footer column heads, the settings caption, who-can-run badges). A stamp never sits above a headline as a kicker or eyebrow.

**The One Lockup Rule.** The display size belongs to the Frost.Byte lockup. Every other heading is a headline or title.

## Layout

The page is the carton's dieline read top to bottom: top flap (brand, links, two actions), front panel, What's inside, the serving suggestion, the back panel, the close, and the bottom flap. Panels are full-bleed bands; content sits in a centered wrap of at most 1240px, with a 16px gutter that widens to 24px from 640px. Panels breathe with 64px of vertical padding, rising to 96px from 960px. A 2px dashed fold line separates adjacent panels.

The front grid splits copy and window about 51/49 from 1024px; the die-cut window holds a fixed-height chat (364px, up to 410px on desktop). Module wrappers run in an auto-fill grid with a 460px minimum track, so identical wrapper units set the rhythm. The back panel puts the 380px Bot Facts column beside the copy from 960px. The leaflet (docs) puts a 250px sticky contents box beside a body capped at 840px from 1024px. Changelog lots put a 170px date-stamp gutter beside the title from 760px and stack below that. Below 480px the top flap drops secondary links, wrappers move their state badge under the text, and settings tables collapse to stacked rows at 640px.

## Elevation & Depth

None. The system is flat printed ink with no box-shadows, drop shadows, gradients, glows or blurs anywhere. Depth reads through ink contrast (carton against board, board against raised board) and through cut and print devices: the die-cut window's 4px frost rim, the wrapper's crimped ends with printed heat-seal ridges, and two flat white cellophane glints at 10% opacity skewed across the window. Flat transparency is part of the print vocabulary (the glints, the 20% ridge ink, the 45% dashed seal line on the state badge). Transparency that imitates light is not.

### Named Rules
**The Flat Ink Rule.** If it needs a shadow, gradient or glow to read, change the ink or add a printed rule instead.

## Shapes

Corners are print-and-cut. Actions and badges are full pills (999px): tabs, flavour names, segment options, toggles and who-can-run badges. Board containers that stand alone take a 14px corner: the sample form and the docs contents box. Small printed chips (inline code, lot stamps, lot dates, the barcode plate) take 4 to 6px. Discs (50%) carry codes and step numbers: the wrapper code disc, the docs module disc, the direction numbers and the Levels tile. The wrapper itself is nearly square-cornered (3px), with its shape coming from the crimped ends, a 16 by 10px chevron mask repeated down each side. Bot Facts and the unwrapped contents are square.

Two silhouettes are signatures. The die-cut window is an arched cut (150px by 72px top corners, 30px bottom; 110/54 and 26 on phones) framed by a 4px frost rim. The price sticker is a 32-point starburst set at a tilt (-8° on the front, 6° at the close).

Lines carry meaning. Dashed lines are folds and seals: between panels, between docs sections and changelog lots, the tear strip, and the edge where a pop opens. Solid navy rules are print structure: flavour heads (3px), table heads, and the Facts bars (1px, 6px and 12px).

## Components

### Buttons
Printed tabs: confident, chunky and flat.
- **Shape:** full pill (999px), 2px border in the current colour, at least 44px tall.
- **Primary (lemon tab):** lemon fill, navy text, Archivo 700 15px, 10px by 18px padding. Hover turns it raised white.
- **Line tab:** transparent with a white outline on carton; hover fills it white with carton text. A navy variant is used on board.
- **Ink tab:** navy fill with white text; hover turns it carton blue.
- **Hover / Focus:** 160ms colour change on the ease-out curve; pressing nudges it 1px down. The focus ring is a 3px lemon outline at a 3px offset on carton, navy on board.

### Price Sticker (signature)
The primary "Add to Discord" action: a 176px lemon starburst tilted -8°, with "Free" in display 800 46px over a wide uppercase call to action. Hover straightens it to -3°, scales it to 1.04 and turns the burst white (260ms). The close repeats it at 220 to 260px, tilted 6°, with a 3px navy stroke on the burst.

### Module Wrapper (signature)
One `<details>` per module. The closed summary is a flavour-ink bar at least 92px tall, with a 56px disc on the left printing the two-letter code (display 800 26px, inverse ink), the module name (display 800 20px) and description in the middle, and an "On / Off at start" stamp badge on the right behind a dashed seal line. The crimped ends carry printed ridges. On hover the right end lifts 3°. Opening it tears that end away (translate 8px/4px, rotate 9°) and the raised-white contents slide out with a clip reveal (340ms). The contents have a 2px navy frame, a dashed top edge, command links and a dashed-row settings list.

### Chips
- **Taste chip:** a mini wrapper in flavour ink with crimped ends, a 28px code disc and the command in mono 600 14px. Pressed state is shown by a white border.
- **Flavour name pill:** flavour ink, inverse print, stamp type.
- **Who-can-run badge:** a 1.5px faded-ink outline pill in stamp type.
- **Lot stamp:** a 2px navy outline, 4px corners, mono 600 13px with wide tracking. It turns lemon when targeted.

### Cards / Containers
- **Corner Style:** 14px for free-standing board containers; square for Bot Facts and unwrapped contents.
- **Background:** raised board on carton or board.
- **Shadow Strategy:** none (see Elevation & Depth).
- **Border:** 2px solid navy.
- **Internal Padding:** 16 to 18px.
- **Bot Facts:** a nutrition-label panel, 380px max, with a heavy 12px bar under the serving line, 1px row rules, a 6px section rule and tabular numerals.

### Inputs / Fields
- **Style:** frost-board fill, 2px ink-line stroke, 8px corners, mono 600 16px values.
- **Hover:** the stroke deepens to faded ink.
- **Focus:** a navy stroke plus a 3px carton-blue outline at a 1px offset.
- **Error:** a cherry stroke and a bold 13px error line.
- **Segments and toggles:** segments are navy-outlined pills that fill navy when selected. The toggle is a 46 by 26px pill track (ink-line, carton when on) with a white knob that slides 20px.

### Navigation
- **Top flap:** the brand lockup (mark plus Bricolage 800 24px with a lemon dot), Archivo 600 15px links, and the line and lemon tabs on the right. Links underline at 2px on hover and when current. A 2px dashed score line runs beneath. Below 900px the links wrap to their own scrolling row; below 480px secondary links hide and "Log in" moves into the row.
- **Bottom flap:** a navy band behind a dashed fold, with a barcode on a raised-white plate, lemon stamp column heads, white links and 13px fine print.
- **Docs contents:** a sticky 14px-cornered board box. Category links carry a 10px flavour dot; module links nest under them from 1024px.

### Mark
A hexagonal ice crystal with a bite out of the upper-right edge and a six-armed flake inside. Carton-blue hexagon with a white flake on the favicon and on carton; inverse and lemon variants come from the same sprite symbol.

## Do's and Don'ts

### Do:
- **Do** print every flavour ink with its label: the two-letter code on a module, the flavour or category name on a category.
- **Do** pair each flavour with its fixed print ink: white on cherry and grape, navy on lemon and lime.
- **Do** separate panels, sections and lots with 2px dashed fold lines, and use solid navy rules for structure inside them.
- **Do** keep lemon for the primary action, selection, focus on carton and targeted highlights, beyond its community-category role.
- **Do** set packaging labels as stamps: Archivo width 125, 800, about 12px, 0.06em tracking, uppercase.
- **Do** keep motion physical and brief on the ease-out curve (cubic-bezier(0.16, 1, 0.3, 1)): 160ms for states, 320 to 340ms for the settle and the unwrap. Reduced motion removes all of it.

### Don't:
- **Don't** add box-shadows, gradients, glows or blurs. Depth is ink, rules and cuts.
- **Don't** use a flavour ink as unlabeled decoration (the registration bar is the only exemption).
- **Don't** put a stamp above a headline as a kicker or eyebrow.
- **Don't** take Discord's window greys or blurple outside the die-cut window.
- **Don't** apply this system to the dashboard app. It keeps the Polar Night look.
