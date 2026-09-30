// The Profile tab: upload a CV, see and edit what it says (edits are stored apart from the
// CV's own reading, so a re-upload never wipes them), and the best-matching jobs. All
// untrusted text (CV contents, job titles) goes into the DOM via textContent.

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
const POLL_MS = 1200;
const POLL_MAX = 25;

/** The strings a list shows: skills and places are objects. */
const labelOf = (field, item) => (field === 'skills' || field === 'locations' ? item.name : item);

export function initProfile({ h, root, whole }) {
  let state = null;
  let matches = null;
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

  async function loadMatches() {
    try { matches = await call(`/api/profile/matches?limit=${MATCHES_SHOWN}`); } catch (e) { matches = null; }
  }

  /** After a change the ranking is recomputed in the background: poll until it's done. */
  function pollMatches(tries = 0) {
    clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => {
      await loadMatches();
      render();
      if (matches && matches.stale && tries < POLL_MAX) pollMatches(tries + 1);
    }, POLL_MS);
  }

  async function refresh() {
    try {
      state = await call('/api/profile');
      await loadMatches();
      error = null;
    } catch (e) { error = e.message; }
    render();
    if (matches && matches.stale) pollMatches();
  }

  async function run(action) {
    busy = true; error = null; render();
    try {
      state = await action();
      await loadMatches();
    } catch (e) { error = e.message; }
    busy = false;
    render();
    if (matches && matches.stale) pollMatches();
  }

  const save = (patch) => run(() => call('/api/profile/overrides', {
    method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(patch),
  }));

  async function upload(file) {
    if (!file) return;
    await run(async () => call(`/api/profile/cv?filename=${encodeURIComponent(file.name)}`, { method: 'POST', body: file }));
  }

  const profile = () => state && state.profile;
  const overridden = (field) => { const p = profile(); return p && p.overrides[field] != null; };

  function listEditor([field, title]) {
    const p = profile();
    const items = p.merged[field] || [];
    const edited = overridden(field);
    const input = h('input', { type: 'text', class: 'chip-input', placeholder: 'add…', 'aria-label': `Add to ${title}`, maxlength: '80' });
    const next = (list) => (field === 'skills'
      ? list.map((i) => (typeof i === 'string' ? { name: i, weight: 0.6 } : i))
      : list.map((i) => labelOf(field, i)));
    const remove = (index) => save({ [field]: next(items.filter((_, j) => j !== index)) });
    input.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter') return;
      const value = input.value.trim();
      if (!value) return;
      e.preventDefault();
      save({ [field]: next([...items, value]) });
    });
    return h('section', { class: 'field' },
      h('h4', {}, title, edited ? h('button', { class: 'btn reset', type: 'button', text: 'reset',
        title: 'Go back to what the CV says', onclick: () => save({ [field]: null }) }) : null),
      h('div', { class: 'chips' },
        ...items.map((item, i) => h('span', { class: `chip${edited ? ' edited' : ''}`,
          title: field === 'skills' ? `weight ${item.weight}` : null },
          labelOf(field, item),
          h('button', { class: 'x', type: 'button', 'aria-label': `Remove ${labelOf(field, item)}`, text: '×', onclick: () => remove(i) }))),
        input));
  }

  function scalars() {
    const p = profile();
    const m = p.merged;
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
    return h('section', { class: 'field' },
      h('h4', {}, 'Best matches', matches.stale ? h('span', { class: 'stale', text: ' updating…' }) : null),
      rows.length
        ? h('ul', { class: 'rows' }, ...rows.map((m) => h('li', {},
            h('span', { class: 'score', text: `${Math.round(m.score * 100)}%` }), ' ',
            h('a', { href: m.url, target: '_blank', rel: 'noopener noreferrer', text: m.title }),
            h('div', { class: 'meta', text: [m.company, m.location, m.remote_mode].filter(Boolean).join(' · ') }),
            m.reasons.length ? h('div', { class: 'meta why', text: m.reasons.join(' · ') }) : null)))
        : h('p', { class: 'empty', text: 'No jobs to rank yet. They appear as the crawler finds them.' }));
  }

  function uploader() {
    const file = h('input', { type: 'file', accept: '.pdf,.md,.markdown,.txt,application/pdf,text/plain,text/markdown', id: 'cv-file', class: 'visually-hidden' });
    file.addEventListener('change', () => { upload(file.files[0]); file.value = ''; });
    const sentTo = state && state.llm && state.llm.cv_sent_to;
    return h('section', { class: 'field' },
      h('label', { class: 'btn upload', for: 'cv-file', text: profile() ? 'Upload a different CV' : 'Upload your CV' }),
      file,
      h('p', { class: 'hint', text: `PDF, Markdown or text, up to 5 MB. ${sentTo
        ? `The CV text is sent to ${sentTo} to be read.`
        : 'It is read on this machine and never leaves it.'}` }));
  }

  function render() {
    const p = profile();
    const parts = [];
    if (error) parts.push(h('p', { class: 'error', role: 'alert', text: error }));
    if (busy) parts.push(h('p', { class: 'hint', text: 'Working…' }));
    if (!p) {
      parts.push(h('p', { class: 'empty', text: 'Give the crawler your CV and it ranks jobs by fit and follows links toward what suits you. Nothing is filtered out; without a CV everything is ranked neutrally.' }));
      parts.push(uploader());
    } else {
      parts.push(h('p', { class: 'facts', text: `${p.name} · read by the ${p.source === 'llm' ? 'LLM' : 'local parser'}. Edits below win over the CV.` }));
      parts.push(uploader());
      parts.push(scalars());
      parts.push(...LISTS.map(listEditor));
      parts.push(matchList());
      parts.push(h('section', { class: 'field' },
        h('button', { class: 'btn', type: 'button', text: 'Stop using this profile',
          onclick: () => run(() => call('/api/profile', { method: 'DELETE' })) })));
    }
    root.replaceChildren(...parts.filter(Boolean));
  }

  render();
  return { refresh };
}
