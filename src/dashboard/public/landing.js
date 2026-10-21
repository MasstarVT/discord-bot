// The landing page's live parts. The window demo: each flavour button plays a sample conversation built from the bot's
// own reply text (the members and numbers are made up); the page shows the /rank one without JavaScript. The serving
// suggestion: a sample Levels form whose preview shows what the bot would post. Nothing is sent anywhere.
// Old dashboard bookmarks lived at "/#/guild/..."; the dashboard now sits at /dashboard, so forward them there.
// Guarded so the file stays import-safe in Node (test/smoke.test.js imports every source file).
if (typeof location !== 'undefined' && location.hash.startsWith('#/')) location.replace(`/dashboard${location.hash}`);

if (typeof document !== 'undefined') {
  const reduceMotion = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

  /** Builds an element; children are nodes or text (never parsed as HTML). */
  const el = (tag, cls, ...kids) => {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    for (const kid of kids.flat()) if (kid !== null && kid !== undefined && kid !== false) node.append(kid);
    return node;
  };

  /** Discord-style text: **bold**, `code`, <@name> mentions and <t:label> timestamps. */
  const rich = (text) => {
    const out = [];
    let last = 0;
    for (const m of String(text).matchAll(/\*\*(.+?)\*\*|`([^`]+)`|<@([\w.]+)>|<t:([^>]+)>/g)) {
      if (m.index > last) out.push(text.slice(last, m.index));
      if (m[1] !== undefined) out.push(el('strong', null, m[1]));
      else if (m[2] !== undefined) out.push(el('code', null, m[2]));
      else if (m[3] !== undefined) out.push(el('span', 'dc-mention', `@${m[3]}`));
      else out.push(el('span', 'dc-ts', m[4]));
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push(text.slice(last));
    return out;
  };

  const markSvg = () => {
    const ns = 'http://www.w3.org/2000/svg';
    const svg = document.createElementNS(ns, 'svg');
    svg.setAttribute('class', 'mark');
    svg.setAttribute('width', '26');
    svg.setAttribute('height', '26');
    const use = document.createElementNS(ns, 'use');
    use.setAttribute('href', '#fb-mark');
    svg.append(use);
    return svg;
  };

  const avatar = (who) => {
    const av = el('span', `dc-av dc-av-${who === 'Frost.Byte' ? 'bot' : who === 'drifter' ? 'spam' : who}`);
    av.setAttribute('aria-hidden', 'true');
    av.append(who === 'Frost.Byte' ? markSvg() : who[0].toUpperCase());
    return av;
  };

  /** One message. opts: reply ({ name, cmd } or { name, quote }), text, embed, image, buttons, reactions, subtext. */
  const message = (who, time, opts = {}) => {
    const isBot = who === 'Frost.Byte';
    const body = el('div', 'dc-body');
    if (opts.reply) {
      const line = el('p', 'dc-reply', el('span', 'dc-reply-name', opts.reply.name));
      if (opts.reply.cmd) line.append(' used ', el('code', 'dc-cmd', opts.reply.cmd));
      else line.append(` ${opts.reply.quote}`);
      body.append(line);
    }
    body.append(el('p', 'dc-meta', el('span', 'dc-name', who), isBot ? el('span', 'dc-bot', 'APP') : null, el('span', 'dc-time', time)));
    if (opts.text) body.append(el('p', 'dc-text', rich(opts.text)));
    if (opts.subtext) body.append(el('p', 'dc-subtext', rich(opts.subtext)));
    if (opts.embed) body.append(opts.embed);
    if (opts.image) {
      const img = el('img', 'dc-attach');
      Object.assign(img, { src: opts.image.src, width: opts.image.width, height: opts.image.height, alt: opts.image.alt });
      body.append(img);
    }
    if (opts.buttons) body.append(opts.buttons);
    if (opts.reactions) body.append(el('div', 'dc-reactions', opts.reactions.map((r) => el('span', 'dc-react', r))));
    return el('li', 'dc-msg', avatar(who), body);
  };

  /** An embed: colour (green/yellow/red/blurple), title, description, inline fields, footer. */
  const embed = ({ colour = 'blurple', title, desc, fields, foot }) => el('div', `dc-embed dc-embed-${colour}`,
    title ? el('p', 'dc-embed-title', rich(title)) : null,
    desc ? el('p', 'dc-embed-desc', rich(desc)) : null,
    fields ? el('div', 'dc-fields', fields.map(([name, value]) => el('div', null, el('p', 'dc-field-name', name), el('p', null, rich(value))))) : null,
    foot ? el('p', 'dc-embed-foot', foot) : null);

  // ── Time helpers for the Auto Timestamps sample ────────────────────────────────────────────────────────────
  /** The instant a wall-clock time in a time zone falls on (two passes cover a DST change). */
  const zonedInstant = (y, mo, d, h, mi, zone) => {
    let t = Date.UTC(y, mo, d, h, mi);
    for (let i = 0; i < 2; i += 1) {
      const p = Object.fromEntries(new Intl.DateTimeFormat('en-US', {
        timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric',
      }).formatToParts(new Date(t)).map((x) => [x.type, x.value]));
      t -= Date.UTC(+p.year, +p.month - 1, +p.day, +p.hour, +p.minute) - Date.UTC(y, mo, d, h, mi);
    }
    return new Date(t);
  };
  /** Next Friday 17:00 in a zone (today counts when it is Friday before 17:00 there). */
  const nextFriday5pm = (zone) => {
    const now = new Date();
    const p = Object.fromEntries(new Intl.DateTimeFormat('en-US', { timeZone: zone, year: 'numeric', month: 'numeric', day: 'numeric', weekday: 'short' })
      .formatToParts(now).map((x) => [x.type, x.value]));
    const dow = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'].indexOf(p.weekday);
    for (let add = 0; add < 8; add += 1) {
      if ((dow + add) % 7 !== 5) continue;
      const at = zonedInstant(+p.year, +p.month - 1, +p.day + add, 17, 0, zone);
      if (at > now) return at;
    }
    return zonedInstant(+p.year, +p.month - 1, +p.day + 7, 17, 0, zone);
  };
  const relative = (date) => {
    const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });
    const hours = Math.round((date - Date.now()) / 3_600_000);
    return Math.abs(hours) < 24 ? rtf.format(hours, 'hour') : rtf.format(Math.round(hours / 24), 'day');
  };

  // ── The window demo ─────────────────────────────────────────────────────────────────────────────────────────
  const log = document.getElementById('demo-log');
  const channel = document.getElementById('demo-channel');
  const caption = document.getElementById('window-cap');
  const tastes = document.getElementById('demo-tastes');
  const BASE = 'A sample server with made-up members and numbers; the replies are Frost.Byte\'s own messages.';

  /** The sample poll: votes per option and the visitor's own vote, if any. */
  const poll = { options: ['Spirited Away', 'The Thing', 'Paddington 2'], votes: [5, 3, 4], mine: -1 };
  const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
  const pollEmbed = () => {
    const total = poll.votes.reduce((a, b) => a + b, 0);
    const lines = poll.options.map((opt, i) => {
      const pct = total ? Math.round((poll.votes[i] / total) * 100) : 0;
      const filled = Math.round(pct / 10);
      return `**${i + 1}.** ${opt}\n${'▰'.repeat(filled)}${'▱'.repeat(10 - filled)} **${pct}%** · ${plural(poll.votes[i], 'vote')}`;
    });
    return embed({
      title: '📊 Movie night: what are we watching?',
      desc: `${lines.join('\n')}\n\n🗳️ **${total}** ${total === 1 ? 'vote' : 'votes'} from **${total}** ${total === 1 ? 'member' : 'members'} · one choice per member\n⏰ Ends <t:${relative(new Date(Date.now() + 2 * 86_400_000))}>\n👤 Started by <@ash>`,
      foot: 'Click a button to vote · click it again to remove your vote',
    });
  };
  const pollButtons = (rerender) => el('div', 'dc-buttons', poll.options.map((opt, i) => {
    const b = el('button', 'dc-btn', `${i + 1}. ${opt}`);
    b.type = 'button';
    b.setAttribute('aria-pressed', String(poll.mine === i));
    b.addEventListener('click', () => {
      if (poll.mine === i) { poll.votes[i] -= 1; poll.mine = -1; } else {
        if (poll.mine >= 0) poll.votes[poll.mine] -= 1;
        poll.votes[i] += 1;
        poll.mine = i;
      }
      rerender(i);
    });
    return b;
  }));

  const SCENES = {
    rank: {
      channel: 'general',
      caption: 'nova checks their rank card, then levels up a few messages later. The card is drawn by the bot.',
      messages: () => [
        message('nova', 'Today at 21:40', { text: 'ok how close am I to level 13' }),
        message('Frost.Byte', 'Today at 21:41', { reply: { name: 'nova', cmd: '/rank' }, image: { src: '/demo/rank-card.png', width: 700, height: 250, alt: 'Rank card for nova: level 12, rank 4 on the server, 1.2K of 1.5K XP towards the next level.' } }),
        message('Frost.Byte', 'Today at 21:58', { text: 'GG <@nova>, you just advanced to level 13!' }),
      ],
    },
    daily: {
      channel: 'bot-commands',
      caption: 'kit claims the daily reward on day 3 of a streak.',
      messages: () => [
        message('Frost.Byte', 'Today at 09:12', {
          reply: { name: 'kit', cmd: '/daily' },
          embed: embed({
            colour: 'green',
            title: '✅  Daily Reward Claimed',
            desc: 'You received 🪙 **85** coins!',
            fields: [['Streak', '🔥 Day 3 (×1.2)'], ['Balance', '🪙 **1,240** coins'], ['Next claim', `<t:${relative(new Date(Date.now() + 86_400_000))}>`]],
            foot: 'Tomorrow: ×1.4 · claim within 48 hours to keep your streak.',
          }),
        }),
      ],
    },
    poll: {
      channel: 'events',
      caption: 'A live poll: vote with the buttons and the bars change, the way they do in Discord.',
      messages: () => {
        const slot = message('Frost.Byte', 'Today at 18:30', { reply: { name: 'ash', cmd: '/poll create' }, embed: pollEmbed() });
        const body = slot.querySelector('.dc-body');
        const draw = (focusIndex) => {
          body.querySelector('.dc-embed').replaceWith(pollEmbed());
          body.querySelector('.dc-buttons')?.remove();
          body.append(pollButtons(draw));
          if (focusIndex !== undefined) body.querySelectorAll('.dc-btn')[focusIndex]?.focus();
        };
        body.append(pollButtons(draw));
        return [slot];
      },
    },
    autotime: {
      channel: 'general',
      caption: () => {
        const zone = Intl.DateTimeFormat().resolvedOptions().timeZone || 'your time zone';
        return `juno has their timezone set to Chicago and types "friday 5pm". You see the time in yours (${zone}), like everyone else in the server.`;
      },
      messages: () => {
        const at = nextFriday5pm('America/Chicago');
        const full = new Intl.DateTimeFormat(undefined, { dateStyle: 'full', timeStyle: 'short' }).format(at);
        return [
          message('juno', 'Today at 14:05', { text: 'movie night friday 5pm, who\'s in?' }),
          message('Frost.Byte', 'Today at 14:05', {
            reply: { name: 'juno', quote: 'movie night friday 5pm, who\'s in?' },
            text: `🕒 \`friday 5pm\` → <t:${full}> (<t:${relative(at)}>)`,
            subtext: 'Everyone sees this in their own timezone.',
          }),
        ];
      },
    },
    counting: {
      channel: 'counting',
      caption: 'A counting channel: a wrong number, or the same member twice in a row, starts it over from 1.',
      messages: () => [
        message('nova', 'Today at 20:11', { text: '41', reactions: ['✅ 1'] }),
        message('kit', 'Today at 20:12', { text: '42', reactions: ['✅ 1'] }),
        message('kit', 'Today at 20:12', { text: '43', reactions: ['❌ 1'] }),
        message('Frost.Byte', 'Today at 20:12', {
          embed: embed({ colour: 'red', title: '❌  Count Ruined', desc: '<@kit> ruined it at **42**! You can\'t count twice in a row.\nStart again from **1**. Best run: **42**.' }),
        }),
      ],
    },
    warn: {
      channel: 'general',
      caption: 'A moderator warns a member. Every warning gets a case number, and enough of them set off the escalation you configure.',
      messages: () => [
        message('drifter', 'Today at 16:40', { text: 'selling cheap accounts, dm me' }),
        message('Frost.Byte', 'Today at 16:41', {
          reply: { name: 'ash', cmd: '/warn' },
          embed: embed({ colour: 'green', title: '✅  Warning Issued', desc: '**User:** <@drifter> `drifter`\n**Reason:** Selling accounts\n**Active warnings:** 1\n**Case:** #17' }),
        }),
      ],
    },
    welcome: {
      channel: 'welcome',
      caption: 'A new member joins, with the optional welcome card switched on.',
      messages: () => [
        message('Frost.Byte', 'Today at 19:02', {
          text: 'Hey <@nova>, welcome to **Night Owls**!',
          embed: embed({ colour: 'green', title: '👋 Welcome to Night Owls!', desc: 'You are our **1,284th** member. Enjoy your stay!' }),
          image: { src: '/demo/welcome-card.png', width: 700, height: 250, alt: 'Welcome card: welcome to Night Owls, nova, member number 1,284.' },
        }),
      ],
    },
  };

  /** Plays a scene: clears the log and adds its messages one by one. */
  let playing = 0;
  const play = (name) => {
    const scene = SCENES[name];
    if (!scene || !log) return;
    const run = ++playing;
    channel.textContent = scene.channel;
    caption.textContent = `${typeof scene.caption === 'function' ? scene.caption() : scene.caption} ${BASE}`;
    log.replaceChildren();
    const msgs = scene.messages();
    msgs.forEach((msg, i) => {
      const add = () => {
        if (run !== playing) return;
        if (!reduceMotion) msg.classList.add('is-new');
        log.append(msg);
        log.scrollTop = log.scrollHeight;
      };
      if (reduceMotion || i === 0) add(); else setTimeout(add, i * 420);
    });
  };

  if (log && tastes) {
    caption.textContent = `${SCENES.rank.caption} ${BASE}`;
    log.scrollTop = log.scrollHeight;
    tastes.hidden = false;
    for (const btn of tastes.querySelectorAll('[data-demo]')) {
      btn.addEventListener('click', () => {
        for (const b of tastes.querySelectorAll('[data-demo]')) b.setAttribute('aria-pressed', String(b === btn));
        play(btn.dataset.demo);
      });
    }
  }

  // ── The serving suggestion: a sample Levels form and what the bot would post ────────────────────────────────
  const form = document.getElementById('levels-demo');
  const ssLog = document.getElementById('ss-log');
  const ssWhere = document.getElementById('ss-where');
  const ssXp = document.getElementById('ss-xp');
  const ssError = document.getElementById('ss-xp-error');
  const SAMPLE = { '{player}': '<@nova>', '{mention}': '<@nova>', '{player.name}': 'nova', '{level}': '13', '{rank}': '4', '{xp}': '18,740' };

  const updatePreview = () => {
    const data = new FormData(form);
    const min = Number(data.get('xpMin'));
    const max = Number(data.get('xpMax'));
    const minInput = form.elements.namedItem('xpMin');
    const maxInput = form.elements.namedItem('xpMax');
    const inRange = (n, input) => Number.isInteger(n) && n >= Number(input.min) && n <= Number(input.max);
    const okMin = inRange(min, minInput);
    const okMax = inRange(max, maxInput) && (!okMin || max >= min);
    minInput.setAttribute('aria-invalid', String(!okMin));
    maxInput.setAttribute('aria-invalid', String(!okMax));
    ssError.hidden = okMin && okMax;
    ssError.textContent = !okMin || !inRange(max, maxInput)
      ? `XP per message must be a whole number from ${minInput.min} to ${minInput.max}.`
      : 'The maximum must be at least the minimum.';
    if (okMin && okMax) ssXp.textContent = min === max ? `nova earns ${min} XP per message.` : `nova earns ${min} to ${max} XP per message.`;

    const mode = data.get('levelUpMode');
    const textarea = form.elements.namedItem('levelUpMessage');
    const template = String(data.get('levelUpMessage') ?? '').trim() || textarea.defaultValue;
    const filled = template.replace(/\{(player\.name|player|mention|level|rank|xp)\}/g, (m) => SAMPLE[m]);
    const pings = /\{(player|mention)\}/.test(template);
    ssWhere.textContent = { CURRENT: 'general', DM: 'Direct message', CHANNEL: 'level-ups' }[mode] ?? 'general';
    ssWhere.previousElementSibling.hidden = mode === 'DM';
    if (mode === 'OFF') {
      ssLog.replaceChildren(el('li', 'ss-off', 'No level-up message is sent. Members still earn XP and level up.'));
      return;
    }
    const msg = data.get('levelUpUseEmbed')
      ? message('Frost.Byte', 'Today at 21:58', { text: pings ? '<@nova>' : null, embed: embed({ colour: 'green', desc: filled }) })
      : message('Frost.Byte', 'Today at 21:58', { text: filled });
    ssLog.replaceChildren(msg);
  };

  if (form && ssLog) {
    form.addEventListener('submit', (e) => e.preventDefault());
    form.addEventListener('input', updatePreview);
    form.addEventListener('change', updatePreview);
    updatePreview();
  }
}
