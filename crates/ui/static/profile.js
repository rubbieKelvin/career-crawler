// The Profile page: every CV profile the user has, which one is active (the one that ranks
// jobs and steers the crawl), and an editor for the selected one. Edits are stored apart from
// the CV's own reading, so a re-upload never wipes them. All untrusted text (CV contents,
// job titles) goes into the DOM via textContent.

const LISTS = [
  ['titles', 'Titles'],
  ['skills', 'Skills'],
  ['industries', 'Industries'],
  ['locations', 'Places'],
  ['languages', 'Languages'],
  ['must_have', 'Must mention'],
  ['exclude', 'Rule out'],
  ['excluded_companies', 'Skip companies'],
];
const SENIORITIES = ['', 'intern', 'junior', 'mid', 'senior', 'lead', 'manager', 'director', 'executive'];
const REMOTE = [['', 'No preference'], ['onsite', 'On-site'], ['hybrid', 'Hybrid'], ['remote', 'Remote']];
const MATCHES_SHOWN = 15;
const SUGGEST_DELAY_MS = 120;
const POLL_MS = 1200;
const POLL_MAX = 25;
const when = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

/** The strings a list shows: skills and places are objects. */
const labelOf = (field, item) => (typeof item === 'string' ? item : item.name);

export function initProfile({ h, root }) {
  let profiles = [];
  let llm = null;
  let selectedId = Number(new URLSearchParams(location.search).get('id')) || null;
  let detail = null;
  let matches = null;
  let renaming = false;
  let error = null;
  let busy = false;
  let pollTimer = null;

  async function call(path, options) {
    const resp = await fetch(path, options);
    let body = null;
    try { body = await resp.json(); } catch (e) { /* not JSON */ }
    if (!resp.ok) throw new Error((body && body.error) || `${resp.status}`);
    return body;
  }
  const json = (method, body) => ({ method, headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });

  function syncUrl() {
    history.replaceState(null, '', selectedId ? `?id=${selectedId}` : location.pathname);
  }

  async function loadList() {
    const body = await call('/api/profiles');
    profiles = body.profiles;
    llm = body.llm;
    // Keep the selection if it still exists; else the active profile, else the first.
    if (!profiles.some((p) => p.id === selectedId)) {
      const pick = profiles.find((p) => p.active) || profiles[0];
      selectedId = pick ? pick.id : null;
    }
  }

  async function loadDetail() {
    detail = null;
    matches = null;
    if (!selectedId) return;
    detail = (await call(`/api/profiles/${selectedId}`)).profile;
    try { matches = await call(`/api/profiles/${selectedId}/matches?limit=${MATCHES_SHOWN}`); } catch (e) { matches = null; }
  }

  /** After a change the active profile's ranking is recomputed in the background: poll until done. */
  function pollMatches(tries = 0) {
    clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => {
      try {
        matches = await call(`/api/profiles/${selectedId}/matches?limit=${MATCHES_SHOWN}`);
        await loadList();
      } catch (e) { return; }
      render();
      if (matches && matches.stale && tries < POLL_MAX) pollMatches(tries + 1);
    }, POLL_MS);
  }

  async function refresh() {
    try {
      await loadList();
      await loadDetail();
      error = null;
    } catch (e) { error = e.message; }
    syncUrl();
    render();
    if (matches && matches.stale) pollMatches();
  }

  /** Runs a change, then reloads the list and the selected profile. */
  async function run(action) {
    busy = true; error = null; render();
    try {
      await action();
      await loadList();
      await loadDetail();
    } catch (e) { error = e.message; }
    busy = false;
    renaming = false;
    syncUrl();
    render();
    if (matches && matches.stale) pollMatches();
  }

  const select = (id) => { selectedId = id; renaming = false; error = null; refresh(); };
  const save = (patch) => run(() => call(`/api/profiles/${selectedId}/overrides`, json('PUT', patch)));
  const activate = (id) => run(() => call(`/api/profiles/${id}/activate`, { method: 'POST' }));
  const deactivate = (id) => run(() => call(`/api/profiles/${id}/deactivate`, { method: 'POST' }));
  const rename = (name) => run(() => call(`/api/profiles/${selectedId}`, json('PATCH', { name })));
  function destroy(p) {
    const extra = p.active ? ' The crawler goes back to ranking everything neutrally.' : '';
    if (!confirm(`Delete "${p.name}"? Its edits and job rankings are removed for good.${extra}`)) return;
    run(() => call(`/api/profiles/${p.id}`, { method: 'DELETE' }));
  }

  async function upload(file) {
    if (!file) return;
    await run(async () => {
      const body = await call(`/api/profile/cv?filename=${encodeURIComponent(file.name)}`, { method: 'POST', body: file });
      selectedId = body.profile.id;
    });
  }

  const overridden = (field) => detail && detail.overrides[field] != null;

  // ---------- the list ----------
  function uploader() {
    const file = h('input', { type: 'file', accept: '.pdf,.md,.markdown,.txt,application/pdf,text/plain,text/markdown', id: 'cv-file', class: 'visually-hidden' });
    file.addEventListener('change', () => { upload(file.files[0]); file.value = ''; });
    const sentTo = llm && llm.cv_sent_to;
    return h('div', { class: 'uploader' },
      h('label', { class: 'btn upload', for: 'cv-file', text: profiles.length ? 'Add another CV' : 'Upload your CV' }),
      file,
      h('p', { class: 'hint', text: `PDF, Markdown or text, up to 5 MB. ${sentTo
        ? `The CV text is sent to ${sentTo} to be read.`
        : 'It is read on this machine and never leaves it.'} Uploading one you've used before brings back its profile and edits.` }));
  }

  function card(p) {
    const meta = [p.titles[0], p.seniority, `${p.skill_count} skills`].filter(Boolean).join(' · ');
    const state = [
      p.edited_fields ? `${p.edited_fields} edited` : null,
      p.active ? (p.matches_stale ? 'ranking jobs…' : `${p.match_count} jobs ranked`) : null,
      p.source === 'llm' ? 'read by LLM' : 'read by parser',
    ].filter(Boolean).join(' · ');
    return h('li', { class: `pcard${p.id === selectedId ? ' selected' : ''}${p.active ? ' is-active' : ''}` },
      h('button', { class: 'pcard-main', type: 'button', 'aria-current': String(p.id === selectedId), onclick: () => select(p.id) },
        h('span', { class: 'pname' }, p.name, p.active ? h('span', { class: 'badge', text: 'active' }) : null),
        h('span', { class: 'pmeta', text: meta }),
        h('span', { class: 'pmeta', text: state }),
        h('span', { class: 'pmeta', text: `updated ${when.format(p.updated_at)}` })),
      h('div', { class: 'pcard-actions' },
        p.active
          ? h('button', { class: 'btn', type: 'button', text: 'deactivate', onclick: () => deactivate(p.id) })
          : h('button', { class: 'btn', type: 'button', text: 'set active', onclick: () => activate(p.id) }),
        h('button', { class: 'btn danger', type: 'button', text: 'delete', onclick: () => destroy(p) })));
  }

  function listPane() {
    return h('div', { class: 'plist' },
      profiles.length
        ? h('ul', { class: 'pcards' }, ...profiles.map(card))
        : h('p', { class: 'empty', text: 'Give the crawler your CV and it ranks jobs by fit and follows links toward what suits you. Nothing is filtered out; without an active profile everything is ranked neutrally.' }),
      uploader(),
      profiles.length && !profiles.some((p) => p.active)
        ? h('p', { class: 'hint', text: 'No profile is active, so jobs are ranked neutrally. Pick one and press “set active”.' })
        : null);
  }

  // ---------- the editor ----------
  /** A search box over the offline city and country table: type, then pick. */
  function placePicker(items, add) {
    const input = h('input', {
      type: 'text', class: 'place-input', placeholder: 'Search a city or country…', role: 'combobox',
      'aria-label': 'Add a place', 'aria-expanded': 'false', 'aria-autocomplete': 'list', autocomplete: 'off', maxlength: '60',
    });
    const list = h('ul', { class: 'suggest', role: 'listbox', hidden: '' });
    let options = [];
    let active = 0;
    let timer = null;
    let asked = 0;
    const taken = new Set(items.map((i) => labelOf('locations', i).toLowerCase()));

    const close = () => { list.hidden = true; input.setAttribute('aria-expanded', 'false'); options = []; };
    const paint = (hint) => {
      list.replaceChildren();
      if (!options.length && hint) list.append(h('li', { class: 'none', text: hint }));
      options.forEach((o, i) => list.append(h('li', { role: 'option', 'aria-selected': String(i === active) },
        h('button', { type: 'button', class: 'opt', tabindex: '-1', text: o.label,
          onmousedown: (e) => { e.preventDefault(); choose(o); } }))));
      list.hidden = !options.length && !hint;
      input.setAttribute('aria-expanded', String(!list.hidden));
    };
    const choose = (o) => { close(); input.value = ''; add(o.value); };

    async function search() {
      const q = input.value.trim();
      if (!q) { close(); return; }
      const mine = ++asked;
      let found = [];
      try { found = await call(`/api/places?q=${encodeURIComponent(q)}&limit=8`); } catch (e) { /* offline: no suggestions */ }
      if (mine !== asked) return; // a newer keystroke is already being answered
      options = found.filter((o) => !taken.has(o.value.toLowerCase()));
      active = 0;
      paint(options.length ? null : `No place called “${q}” in our list. Try a nearby city or the country.`);
    }
    input.addEventListener('input', () => { clearTimeout(timer); timer = setTimeout(search, SUGGEST_DELAY_MS); });
    input.addEventListener('keydown', (e) => {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        if (!options.length) return;
        e.preventDefault();
        active = (active + (e.key === 'ArrowDown' ? 1 : options.length - 1)) % options.length;
        paint();
      } else if (e.key === 'Enter') {
        e.preventDefault();
        if (options[active]) choose(options[active]);
      } else if (e.key === 'Escape') {
        close();
      }
    });
    input.addEventListener('blur', () => setTimeout(close, 100));
    return h('div', { class: 'picker' }, input, list);
  }

  function listEditor([field, title]) {
    const items = detail.merged[field] || [];
    const edited = overridden(field);
    const input = field === 'locations'
      ? null
      : h('input', { type: 'text', class: 'chip-input', placeholder: 'add…', 'aria-label': `Add to ${title}`, maxlength: '80' });
    const next = (list) => (field === 'skills'
      ? list.map((i) => (typeof i === 'string' ? { name: i, weight: 0.6 } : i))
      : list.map((i) => labelOf(field, i)));
    const remove = (index) => save({ [field]: next(items.filter((_, j) => j !== index)) });
    if (input) {
      input.addEventListener('keydown', (e) => {
        if (e.key !== 'Enter') return;
        const value = input.value.trim();
        if (!value) return;
        e.preventDefault();
        save({ [field]: next([...items, value]) });
      });
    }
    const adder = input || placePicker(items, (value) => save({ [field]: next([...items, value]) }));
    return h('section', { class: 'field' },
      h('h4', {}, title, edited ? h('button', { class: 'btn reset', type: 'button', text: 'reset',
        title: 'Go back to what the CV says', onclick: () => save({ [field]: null }) }) : null),
      h('div', { class: 'chips' },
        ...items.map((item, i) => h('span', { class: `chip${edited ? ' edited' : ''}`,
          title: field === 'skills' ? `weight ${item.weight}` : null },
          labelOf(field, item),
          h('button', { class: 'x', type: 'button', 'aria-label': `Remove ${labelOf(field, item)}`, text: '×', onclick: () => remove(i) }))),
        field === 'locations' ? null : input),
      field === 'locations' ? adder : null,
      field === 'locations' && !items.length
        ? h('p', { class: 'hint', text: 'Where you live or would work. Jobs near these places rank higher.' })
        : null);
  }

  function scalars() {
    const m = detail.merged;
    const select = (field, options, value) => {
      const el = h('select', { 'aria-label': field }, ...options.map(([v, label]) => h('option', { value: v, text: label })));
      el.value = value || '';
      el.addEventListener('change', () => save({ [field]: el.value === '' ? null : el.value }));
      return el;
    };
    const number = (field, value, step) => {
      const el = h('input', { type: 'number', min: '0', step, class: 'num', 'aria-label': field });
      if (value != null) el.value = value;
      el.addEventListener('change', () => save({ [field]: el.value === '' ? null : Number(el.value) }));
      return el;
    };
    const relocate = h('input', { type: 'checkbox', id: 'p-relocate' });
    relocate.checked = m.relocate;
    relocate.addEventListener('change', () => save({ relocate: relocate.checked }));
    return h('section', { class: 'field scalars' },
      h('label', {}, 'Seniority', select('seniority', SENIORITIES.map((s) => [s, s || 'Unknown']), m.seniority)),
      h('label', {}, 'Work style', select('remote', REMOTE, m.remote)),
      h('label', {}, 'Years of experience', number('years_experience', m.years_experience, '1')),
      h('label', {}, 'Salary expectation (USD / year)', number('salary_expectation_usd', m.salary_expectation_usd, '1000')),
      h('label', { class: 'check', for: 'p-relocate' }, relocate, ' Open to relocating'));
  }

  function matchList() {
    if (!matches || !matches.profile) return null;
    const rows = matches.matches;
    if (!detail.active && !rows.length) {
      return h('section', { class: 'field' }, h('h4', { text: 'Best matches' }),
        h('p', { class: 'empty', text: 'Set this profile active to rank the jobs by it.' }));
    }
    return h('section', { class: 'field' },
      h('h4', {}, 'Best matches', matches.stale ? h('span', { class: 'stale', text: ' updating…' }) : null,
        !detail.active ? h('span', { class: 'stale', text: ' from when it was last active' }) : null),
      rows.length
        ? h('ul', { class: 'rows' }, ...rows.map((m) => h('li', {},
            h('span', { class: 'score', text: `${Math.round(m.score * 100)}%` }), ' ',
            h('a', { href: m.url, target: '_blank', rel: 'noopener noreferrer', text: m.title }),
            h('div', { class: 'meta', text: [m.company, m.location, m.remote_mode].filter(Boolean).join(' · ') }),
            m.reasons.length ? h('div', { class: 'meta why', text: m.reasons.join(' · ') }) : null)))
        : h('p', { class: 'empty', text: 'No jobs to rank yet. They appear as the crawler finds them.' }));
  }

  function nameRow() {
    if (renaming) {
      const input = h('input', { type: 'text', class: 'name-input', value: detail.name, maxlength: '80', 'aria-label': 'Profile name' });
      const commit = () => { const v = input.value.trim(); if (v && v !== detail.name) rename(v); else { renaming = false; render(); } };
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') commit();
        if (e.key === 'Escape') { renaming = false; render(); }
      });
      setTimeout(() => { input.focus(); input.select(); }, 0);
      return h('div', { class: 'name-row' }, input,
        h('button', { class: 'btn', type: 'button', text: 'save', onclick: commit }),
        h('button', { class: 'btn', type: 'button', text: 'cancel', onclick: () => { renaming = false; render(); } }));
    }
    return h('div', { class: 'name-row' },
      h('h3', { text: detail.name }),
      detail.active ? h('span', { class: 'badge', text: 'active' }) : null,
      h('button', { class: 'btn', type: 'button', text: 'rename', onclick: () => { renaming = true; render(); } }));
  }

  function editorPane() {
    if (!detail) {
      return h('div', { class: 'pedit' }, h('p', { class: 'empty', text: profiles.length ? 'Loading…' : 'No profile yet.' }));
    }
    return h('div', { class: 'pedit' },
      nameRow(),
      h('p', { class: 'facts', text: `Read by the ${detail.source === 'llm' ? 'LLM' : 'local parser'} · added ${when.format(detail.created_at)}. Edits below win over the CV.` }),
      scalars(),
      ...LISTS.map(listEditor),
      matchList());
  }

  function render() {
    const parts = [];
    // One reserved line, so the page doesn't jump while a change is in flight.
    parts.push(error
      ? h('p', { class: 'status error', role: 'alert', text: error })
      : h('p', { class: 'status hint', text: busy ? 'Working…' : '' }));
    parts.push(h('div', { class: 'profiles' }, listPane(), profiles.length ? editorPane() : null));
    root.replaceChildren(...parts);
  }

  render();
  return { refresh };
}
