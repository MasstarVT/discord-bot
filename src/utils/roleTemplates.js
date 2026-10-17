// Reaction-role templates that create their roles: plain role sets (pronouns, colors, age groups, ...) and the timezone
// sets of TIMEZONE_ROLE_TEMPLATES, whose roles are also linked to their zones for /timestamp. Used by /reactionrole create
// template:<key>, the dashboard's create-only Template field and the rr.roleTemplate task; roleTemplateSetup.js creates them.
import { TIMEZONE_ROLE_TEMPLATES, TIMEZONE_PANEL } from './timezones.js';

// `name` is the Discord role (reused by name, ignoring case); `label` (≤ 80) and `description` (≤ 100, dropdowns only) are
// the panel option; `emoji` is one standard emoji or null; `color` is a new role's color (0 = none). At most 20 roles.
const role = (name, emoji = null, { color = 0, label = name, description = null } = {}) =>
  Object.freeze({ name, label, emoji, color, description });

const roleSet = ({ label, panel, roles, note = null }) => Object.freeze({
  label,
  panel: Object.freeze({ placeholder: null, ...panel }),
  roles: Object.freeze(roles),
  note,
  linksZones: false,
});

// Mid-tone colors, readable on both the dark and the light Discord theme (so no black or white).
const ROLE_SETS = {
  pronouns: roleSet({
    label: 'Pronouns',
    panel: {
      type: 'NORMAL', layout: 'BUTTONS', title: '💬 Pronoun Roles',
      description: 'Pick the pronouns you go by. You can hold more than one.', placeholder: 'Pick your pronouns',
    },
    roles: [
      role('He/Him', '🩵'),
      role('She/Her', '🩷'),
      role('They/Them', '💜'),
      role('Any Pronouns', '🤍'),
      role('Ask My Pronouns', '💬'),
    ],
  }),
  colors: roleSet({
    label: 'Colors',
    panel: {
      type: 'UNIQUE', layout: 'SELECT', title: '🎨 Color Roles',
      description: 'Pick a color for your name. You can hold one color at a time; pick another to switch.',
      placeholder: 'Pick a color',
    },
    roles: [
      role('Red', '❤️', { color: 0xEF4444 }),
      role('Orange', '🧡', { color: 0xF97316 }),
      role('Yellow', '💛', { color: 0xEAB308 }),
      role('Green', '💚', { color: 0x22C55E }),
      role('Light Blue', '🩵', { color: 0x38BDF8 }),
      role('Blue', '💙', { color: 0x3B82F6 }),
      role('Purple', '💜', { color: 0xA855F7 }),
      role('Pink', '🩷', { color: 0xEC4899 }),
      role('Brown', '🤎', { color: 0xB45309 }),
      role('Grey', '🩶', { color: 0x9CA3AF }),
    ],
    note: 'A color only shows when its role sits above a member\'s other colored roles, so drag the color roles up in '
      + '**Server Settings → Roles** (they stay below my highest role).',
  }),
  age: roleSet({
    label: 'Age groups',
    panel: {
      type: 'UNIQUE', layout: 'BUTTONS', title: '🎂 Age Roles',
      description: 'Pick your age group. You can hold one at a time.', placeholder: 'Pick your age group',
    },
    // Discord's terms set the minimum age at 13, so there is no younger group.
    roles: [role('13-17'), role('18-24'), role('25-34'), role('35-44'), role('45+')],
  }),
  regions: roleSet({
    label: 'Regions',
    panel: {
      type: 'UNIQUE', layout: 'BUTTONS', title: '🗺️ Region Roles',
      description: 'Pick the part of the world you live in.', placeholder: 'Pick your region',
    },
    roles: [
      role('North America', '🌎'),
      role('South America', '🌴'),
      role('Europe', '🇪🇺'),
      role('Africa', '🌍'),
      role('Middle East', '🏜️'),
      role('Asia', '🌏'),
      role('Oceania', '🦘'),
    ],
  }),
  platforms: roleSet({
    label: 'Gaming platforms',
    panel: {
      type: 'NORMAL', layout: 'BUTTONS', title: '🎮 Platform Roles',
      description: 'Pick the platforms you play on. You can hold more than one.', placeholder: 'Pick your platforms',
    },
    roles: [
      role('PC', '🖥️'),
      role('PlayStation', '🎮'),
      role('Xbox', '🟢'),
      role('Nintendo Switch', '🔴'),
      role('Mobile', '📱'),
    ],
  }),
  dms: roleSet({
    label: 'DM status',
    panel: {
      type: 'UNIQUE', layout: 'BUTTONS', title: '✉️ DM Roles',
      description: 'Let others know whether you are open to direct messages.', placeholder: 'Pick your DM status',
    },
    roles: [role('DMs Open', '✅'), role('Ask to DM', '💬'), role('DMs Closed', '⛔')],
  }),
  pings: roleSet({
    label: 'Ping roles',
    panel: {
      type: 'NORMAL', layout: 'BUTTONS', title: '🔔 Ping Roles',
      description: 'Pick what you want to be pinged for. You can hold as many as you like.', placeholder: 'Pick your pings',
    },
    roles: [
      role('Announcements', '📢'),
      role('Events', '📅'),
      role('Giveaways', '🎁'),
      role('Polls', '📊'),
      role('Streams', '🎥'),
    ],
    note: 'The roles are created not mentionable, so only members allowed to mention @everyone can ping them.',
  }),
};

const TIMEZONE_NOTE = 'Each role is linked to its timezone, so `/timestamp` reads a member\'s times in the zone they pick; '
  + 'a member\'s own `/timezone set` still wins over the role.';

/** Every role template by key: the role sets, then the timezone sets (always a Unique panel). */
export const ROLE_TEMPLATES = Object.freeze({
  ...ROLE_SETS,
  ...Object.fromEntries(Object.entries(TIMEZONE_ROLE_TEMPLATES).map(([key, t]) => [key, Object.freeze({
    label: t.label,
    panel: TIMEZONE_PANEL,
    roles: t.roles,
    note: TIMEZONE_NOTE,
    linksZones: true,
  })])),
});

/** The template of that key, or null. */
export function getRoleTemplate(key) {
  return typeof key === 'string' && Object.hasOwn(ROLE_TEMPLATES, key) ? ROLE_TEMPLATES[key] : null;
}
