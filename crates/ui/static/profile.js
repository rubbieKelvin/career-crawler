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
const MATCHES_PAGE = 20;
const POSTED = [['', 'Any time'], ['1', 'Last 24 hours'], ['7', 'Last 7 days'], ['30', 'Last 30 days']];
const NEW_DAYS = 3;
const SORTS = [['score', 'Best match'], ['recent', 'Most recent'], ['salary', 'Highest salary']];
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
  let tab = 'jobs';
  const mf = { q: '', min_score: '', remote: '', category: '', seniority: '', country: '', posted_days: '', sort: 'score', has_salary: '', offset: 0 };
  const resetFilters = () => Object.assign(mf, { q: '', min_score: '', remote: '', category: '', seniority: '', country: '', posted_days: '', sort: 'score', has_salary: '', offset: 0 });
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

  function fetchMatches() {
    const p = new URLSearchParams({ limit: MATCHES_PAGE });
    for (const [k, v] of Object.entries(mf)) if (v !== '' && v != null) p.set(k, v);
    return call(`/api/profiles/${selectedId}/matches?${p}`);
  }

  async function loadDetail() {
    detail = null;
    matches = null;
    if (!selectedId) return;
    detail = (await call(`/api/profiles/${selectedId}`)).profile;
    try { matches = await fetchMatches(); } catch (e) { matches = null; }
  }

  /** After a change the active profile's ranking is recomputed in the background: poll until done. */
  function pollMatches(tries = 0) {
    clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => {
      try {
        matches = await fetchMatches();
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

  const select = (id) => { selectedId = id; resetFilters(); matchBox = null; renaming = false; error = null; refresh(); };
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

  // The jobs view keeps its own DOM so typing in the filter box isn't interrupted by a re-render.
  let matchBox = null;
  let matchTimer = null;
  let matchSeq = 0;

  function reloadMatches() {
    clearTimeout(matchTimer);
    matchTimer = setTimeout(async () => {
      const seq = ++matchSeq;
      try {
        const body = await fetchMatches();
        if (seq !== matchSeq) return;
        matches = body;
      } catch (e) { if (seq === matchSeq) error = e.message; }
      renderMatchResults();
      if (matches && matches.stale) pollMatches();
    }, 200);
  }

  function filterSelect(key, options) {
    const el = h('select', { class: 'chip-select', 'aria-label': key });
    for (const [v, label] of options) el.append(h('option', { value: v, text: label }));
    el.value = mf[key];
    el.addEventListener('change', () => { mf[key] = el.value; mf.offset = 0; reloadMatches(); });
    return el;
  }

  const facetSelects = {};
  function facetSelect(key, anyLabel) {
    const el = h('select', { class: 'chip-select', 'aria-label': anyLabel });
    el.addEventListener('change', () => { mf[key] = el.value; mf.offset = 0; reloadMatches(); });
    facetSelects[key] = { el, anyLabel };
    return el;
  }
  /** Options come from what this profile's jobs actually have; the current pick always stays. */
  function fillFacets(facets) {
    const sources = { category: facets.categories, seniority: facets.seniorities, country: facets.countries };
    for (const [key, { el, anyLabel }] of Object.entries(facetSelects)) {
      const list = sources[key] || [];
      const options = [['', anyLabel], ...list.map(([v, n]) => [v, `${key === 'country' ? v : v.replace(/_/g, ' ')} (${n})`])];
      if (mf[key] && !list.some(([v]) => v === mf[key])) options.push([mf[key], mf[key]]);
      el.replaceChildren(...options.map(([v, label]) => h('option', { value: v, text: label })));
      el.value = mf[key];
    }
  }

  function buildMatchBox() {
    const search = h('input', { type: 'search', placeholder: 'Filter by title, company or place', value: mf.q, 'aria-label': 'Filter jobs' });
    search.addEventListener('input', () => { mf.q = search.value; mf.offset = 0; reloadMatches(); });
    const salary = h('input', { type: 'checkbox', id: 'mf-salary' });
    salary.checked = mf.has_salary === 'true';
    salary.addEventListener('change', () => { mf.has_salary = salary.checked ? 'true' : ''; mf.offset = 0; reloadMatches(); });
    const clear = h('button', { class: 'btn', type: 'button', text: 'Clear filters', hidden: '' });
    clear.addEventListener('click', () => { resetFilters(); matchBox = null; render(); reloadMatches(); });
    matchBox = h('div', { class: 'matchbox' },
      h('div', { class: 'match-summary' }),
      h('div', { class: 'search-form' }, search),
      h('div', { class: 'chips search-chips' },
        facetSelect('category', 'Any category'),
        facetSelect('seniority', 'Any level'),
        facetSelect('country', 'Any country'),
        filterSelect('remote', [['', 'Any work style'], ...REMOTE.slice(1)]),
        filterSelect('posted_days', POSTED),
        filterSelect('sort', SORTS),
        h('label', { class: 'chip-label', for: 'mf-salary' }, salary, ' Has salary'),
        clear),
      h('div', { class: 'match-results' }));
    return matchBox;
  }

  const money = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 0 });
  const posted = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium' });

  function matchRow(m) {
    const tier = m.score >= 0.7 ? 'strong' : m.score >= 0.5 ? 'good' : 'weak';
    const fresh = m.posted_at && Date.now() - m.posted_at < NEW_DAYS * 86400000;
    const meta = [m.location, m.remote_mode, m.seniority, m.category && m.category.replace(/_/g, ' '), m.employment_type && m.employment_type.replace('_', ' ')].filter(Boolean);
    const extra = [m.salary_usd_annual ? `~$${money.format(m.salary_usd_annual)}/yr` : null, m.posted_at ? posted.format(m.posted_at) : null].filter(Boolean);
    return h('li', { class: `match ${tier}` },
      h('span', { class: 'score', text: `${Math.round(m.score * 100)}%` }), ' ',
      h('a', { href: m.url, target: '_blank', rel: 'noopener noreferrer', text: m.title }),
      fresh ? h('span', { class: 'badge', text: 'new' }) : null,
      m.company ? h('div', { class: 'meta' }, m.domain
        ? h('a', { href: `/graph?host=${encodeURIComponent(m.domain)}`, text: m.company })
        : m.company) : null,
      meta.length ? h('div', { class: 'meta', text: meta.join(' · ') }) : null,
      extra.length ? h('div', { class: 'meta', text: extra.join(' · ') }) : null,
      m.reasons.length ? h('div', { class: 'chips reasons' }, ...m.reasons.map((r) => h('span', { class: 'chip', text: r }))) : null);
  }

  /** Strong / good / all, as quick score filters with the counts they'd show. */
  function renderSummary(facets) {
    const box = matchBox.querySelector('.match-summary');
    const tiers = [['0.7', 'Strong matches', facets.strong], ['0.5', 'Good or better', facets.good], ['', 'All ranked', facets.total]];
    box.replaceChildren(...tiers.map(([min, label, n]) => {
      const btn = h('button', { class: 'tier', type: 'button', 'aria-pressed': String(mf.min_score === min) },
        h('span', { class: 'n', text: String(n) }), h('span', { text: label }));
      btn.addEventListener('click', () => { mf.min_score = min; mf.offset = 0; reloadMatches(); });
      return btn;
    }));
  }

  function renderMatchResults() {
    const box = matchBox && matchBox.querySelector('.match-results');
    if (!box) return;
    if (!matches || !matches.profile) { box.replaceChildren(); return; }
    if (matches.facets) { fillFacets(matches.facets); renderSummary(matches.facets); }
    const dirty = ['q', 'min_score', 'remote', 'category', 'seniority', 'country', 'posted_days', 'has_salary'].some((k) => mf[k] !== '');
    matchBox.querySelector('.chips .btn').hidden = !dirty;
    const rows = matches.matches;
    const from = rows.length ? mf.offset + 1 : 0;
    const to = mf.offset + rows.length;
    const prev = h('button', { class: 'btn', type: 'button', text: '← Previous' });
    const next = h('button', { class: 'btn', type: 'button', text: 'Next →' });
    prev.disabled = mf.offset <= 0;
    next.disabled = to >= matches.total;
    prev.addEventListener('click', () => { mf.offset = Math.max(0, mf.offset - MATCHES_PAGE); reloadMatches(); });
    next.addEventListener('click', () => { mf.offset += MATCHES_PAGE; reloadMatches(); });
    box.replaceChildren(
      h('p', { class: 'hint', text: [
        `${matches.total} matching ${matches.total === 1 ? 'job' : 'jobs'}` + (rows.length ? ` · showing ${from}–${to}` : ''),
        matches.stale ? 'updating…' : null,
        !detail.active ? 'from when this profile was last active' : null].filter(Boolean).join(' · ') }),
      rows.length
        ? h('ul', { class: 'rows match-rows' }, ...rows.map(matchRow))
        : h('p', { class: 'empty', text: !detail.active ? 'Set this profile active to rank the jobs by it.' : 'No jobs match. Loosen the filters, or wait for the crawler to find more.' }),
      rows.length ? h('div', { class: 'pager' }, prev, next) : null);
  }

  function matchList() {
    if (!matches || !matches.profile) return null;
    const box = matchBox || buildMatchBox();
    renderMatchResults();
    return box;
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
      h('div', { class: 'tabs' },
        ...[['jobs', 'Matching jobs'], ['profile', 'Edit profile']].map(([key, label]) => h('button', {
          class: 'tab', type: 'button', role: 'tab', 'aria-selected': String(tab === key), text: label,
          onclick: () => { tab = key; render(); },
        }))),
      ...(tab === 'jobs'
        ? [matchList()]
        : [scalars(), ...LISTS.map(listEditor)]));
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
