// The public command and module reference (/docs): per catalog category, each module's switch, setup command,
// settings with their defaults and its slash commands; then the core commands. Built from src/dashboard/catalog.js
// and src/dashboard/commands.json. Pure: no I/O.
import { WHO_LABEL, escapeHtml, flavourOf, publicDescription, publicNote, scalarSettings } from './siteParts.js';

/** The anchor of a module and of a command on /docs. */
export const moduleId = (key) => `mod-${key}`;
export const commandId = (name) => `cmd-${name}`;

/** One command: name, who may run it, description and subcommands. */
function renderCommand(cmd) {
  const subs = cmd.subcommands.length
    ? `<ul class="leaf-subs">${cmd.subcommands.map((s) => `<li><code>/${escapeHtml(cmd.name)} ${escapeHtml(s.name)}</code> ${escapeHtml(publicDescription(s.description))}</li>`).join('')}</ul>`
    : '';
  return [
    `<li class="leaf-cmd" id="${escapeHtml(commandId(cmd.name))}">`,
    '<div class="leaf-cmd-head">',
    `<code class="leaf-cmd-name">/${escapeHtml(cmd.name)}</code>`,
    `<span class="leaf-who">${escapeHtml(WHO_LABEL[cmd.who] ?? cmd.who)}</span>`,
    '</div>',
    `<p class="leaf-cmd-desc">${escapeHtml(publicDescription(cmd.description))}</p>`,
    subs,
    '</li>',
  ].join('');
}

/**
 * The sidebar contents list: getting started, each category with its modules, core commands, help.
 * @param {ReturnType<import('./catalog.js').buildCatalog>} catalog
 */
export function renderDocsToc(catalog) {
  const cats = catalog.categories.map((c) => [
    `<li class="toc-cat toc-${flavourOf(c.id).cls}"><a href="#cat-${escapeHtml(c.id)}">${escapeHtml(c.label)}</a>`,
    `<ul>${c.modules.map((k) => `<li><a href="#${escapeHtml(moduleId(k))}">${escapeHtml(catalog.modules[k].label)}</a></li>`).join('')}</ul>`,
    '</li>',
  ].join('')).join('');
  return `<li><a href="#start">Getting started</a></li><li><a href="#who">Who can run what</a></li>${cats}<li><a href="#core">Core commands</a></li><li><a href="#help">Help and support</a></li>`;
}

/**
 * Every module, grouped by category.
 * @param {ReturnType<import('./catalog.js').buildCatalog>} catalog
 * @param {{ name: string, description: string, module: string|null, who: string, subcommands: object[] }[]} commands
 */
export function renderDocsModules(catalog, commands) {
  return catalog.categories.map((c) => {
    const flavour = flavourOf(c.id);
    const mods = c.modules.map((key) => {
      const mod = catalog.modules[key];
      const cmds = commands.filter((cmd) => cmd.module === key);
      const settings = scalarSettings(mod);
      const switchLine = mod.default
        ? `On when the bot joins. Turn it off with <code>/module disable ${escapeHtml(key)}</code>.`
        : `Off until you run <code>/module enable ${escapeHtml(key)}</code> or flip its switch on the dashboard.`;
      const setup = mod.configCommand ? ` Set it up with <code>${escapeHtml(mod.configCommand)}</code>.` : '';
      const table = settings.length
        ? ['<table class="leaf-settings"><caption>Settings and their defaults</caption>',
          '<thead><tr><th scope="col">Setting</th><th scope="col">Default</th><th scope="col">What it does</th></tr></thead><tbody>',
          settings.map(({ field, value }) => `<tr><th scope="row">${escapeHtml(field.label)}</th><td>${escapeHtml(value)}</td><td>${escapeHtml(publicNote(field.description))}</td></tr>`).join(''),
          '</tbody></table>'].join('')
        : '<p class="leaf-note">Configured with its commands and the lists on the dashboard.</p>';
      return [
        `<section class="leaf-mod" id="${escapeHtml(moduleId(key))}" aria-labelledby="${escapeHtml(moduleId(key))}-h">`,
        `<h3 class="leaf-mod-title" id="${escapeHtml(moduleId(key))}-h"><span class="leaf-code">${escapeHtml(mod.code)}</span>${escapeHtml(mod.label)}</h3>`,
        `<p class="leaf-mod-desc">${escapeHtml(mod.description)}</p>`,
        `<p class="leaf-switch">${switchLine}${setup}</p>`,
        table,
        cmds.length ? `<ul class="leaf-cmds">${cmds.map(renderCommand).join('')}</ul>` : '',
        '</section>',
      ].join('');
    }).join('\n');
    return [
      `<section class="leaf-cat leaf-${flavour.cls}" id="cat-${escapeHtml(c.id)}" aria-labelledby="cat-${escapeHtml(c.id)}-h">`,
      `<h2 class="leaf-cat-title" id="cat-${escapeHtml(c.id)}-h">${escapeHtml(c.label)}</h2>`,
      `<p class="leaf-cat-desc">${escapeHtml(c.description)}</p>`,
      mods,
      '</section>',
    ].join('\n');
  }).join('\n');
}

/** The commands that belong to no module (always on). */
export function renderCoreCommands(commands) {
  return `<ul class="leaf-cmds">${commands.filter((c) => c.module === null).map(renderCommand).join('')}</ul>`;
}
