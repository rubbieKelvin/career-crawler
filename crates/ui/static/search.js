// The Search tab: describe the job you want, and the app reads it — through the LLM when one
// is configured, as keywords when not — into a set of filters it shows as chips. Editing a
// chip re-runs the same route with the filter set you can see, which is how you fix a reading
// you disagree with. All untrusted text (job titles, companies) goes into the DOM through
// textContent.

const CATEGORIES = ['engineering', 'data', 'product', 'design', 'sales', 'marketing',
  'customer_support', 'operations', 'finance', 'hr', 'legal', 'security', 'it', 'other'];
const REMOTE = [['any', 'Any work style'], ['remote', 'Remote'], ['onsite', 'On-site'], ['hybrid', 'Hybrid']];
const SORTS = [['', 'Best of both'], ['relevance', 'Best match of the words'], ['salary_desc', 'Best paid'],
  ['recent', 'Newest'], ['match', 'Best fit for my CV']];
const PLACEHOLDER = 'e.g. nice paying jobs in tech around Lagos';
/** How many hits the panel draws (the search itself returns more). */
const SHOWN = 40;

/** The chip labels for the filters that are not lists. */
function scalarChips(q) {
  const chips = [];
  if (q.near) {
    chips.push({ key: 'near', label: `near ${q.near.place} (${Math.round(q.near.radius_km)} km)`, value: q.near });
  }
  if (q.salary) {
    chips.push({
      key: 'salary',
      label: q.salary.mode === 'top_percentile'
        ? `top ${Math.round(q.salary.value * 100)}% pay`
        : `at least $${Math.round(q.salary.value / 1000)}k/yr`,
      value: q.salary,
    });
  }
  if (q.posted_within_days) {
    chips.push({ key: 'posted_within_days', label: `posted within ${q.posted_within_days} days`, value: q.posted_within_days });
  }
  return chips;
}

export function initSearch({ h, root, whole, salary }) {
  let result = null;
  let error = null;
  let busy = false;
  let text = '';

  const input = h('input', {
    id: 'search-text', type: 'search', placeholder: PLACEHOLDER, autocomplete: 'off',
    'aria-label': 'Describe the job you are looking for',
  });
  input.addEventListener('input', () => { text = input.value; });

  async function call(path, options) {
    const resp = await fetch(path, options);
    let body = null;
    try { body = await resp.json(); } catch (e) { /* not JSON */ }
    if (!resp.ok) throw new Error((body && body.error) || `${resp.status}`);
    return body;
  }

  async function run(action) {
    busy = true; error = null; render();
    try {
      result = await action();
    } catch (e) { error = e.message; }
    busy = false;
    render();
  }

  const post = (payload) => call('/api/search/nl', {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
  });
  /** The words: read by the LLM (or as keywords without one). */
  const ask = (words) => run(() => post({ query: words }));
  /** A change to the filters we already have: no LLM is involved. */
  const refine = (filters) => run(() => post({ filters }));

  const query = () => (result && result.query) || null;
  const edited = (key, value) => {
    const next = { ...query() };
    if (value === null || value === undefined || (Array.isArray(value) && !value.length)) delete next[key];
    else next[key] = value;
    refine(next);
  };
  const without = (key, index) => {
    const current = query()[key];
    if (index === undefined) edited(key, null);
    else edited(key, current.filter((_, i) => i !== index));
  };

  // ---------- parts ----------
  function form() {
    const button = h('button', { class: 'btn', type: 'submit', text: busy ? 'Searching…' : 'Search' });
    button.disabled = busy;
    return h('form', {
      class: 'search-form',
      onsubmit: (e) => { e.preventDefault(); if (text.trim()) ask(text.trim()); },
    }, input, button);
  }

  function chip(label, onRemove, extra) {
    return h('span', { class: 'chip' }, label, extra,
      onRemove ? h('button', { class: 'x', type: 'button', 'aria-label': `Remove ${label}`, text: '×', onclick: onRemove }) : null);
  }

  function listChips(key) {
    const items = (query() && query()[key]) || [];
    return items.map((item, i) => chip(item, () => without(key, i)));
  }

  /** The chip input that appends a word to `key`. */
  function addWord(key, placeholder, maxlength) {
    const field = h('input', { type: 'text', class: 'chip-input', placeholder, 'aria-label': placeholder, maxlength });
    field.addEventListener('keydown', (e) => {
      if (e.key !== 'Enter') return;
      e.preventDefault();
      const word = field.value.trim();
      if (word) edited(key, [...(query()[key] || []), word]);
    });
    return field;
  }

  function addCategory() {
    const select = h('select', { class: 'chip-select', 'aria-label': 'Add a category' },
      h('option', { value: '', text: 'add a field…' }),
      ...CATEGORIES.filter((c) => !((query().categories || []).includes(c))).map((c) => h('option', { value: c, text: c.replace('_', ' ') })));
    select.addEventListener('change', () => {
      if (select.value) edited('categories', [...(query().categories || []), select.value]);
    });
    return select;
  }

  function nearChip() {
    const near = query().near;
    const radius = h('input', { type: 'number', class: 'chip-num', min: '1', max: '500', step: '10', value: Math.round(near.radius_km), 'aria-label': 'Radius in kilometres' });
    radius.addEventListener('change', () => edited('near', { ...near, radius_km: Number(radius.value) }));
    return chip(`near ${near.place}`, () => without('near'), radius, h('span', { text: ' km' }));
  }

  function remoteSelect() {
    const select = h('select', { class: 'chip-select', 'aria-label': 'Work style' },
      ...REMOTE.map(([value, label]) => h('option', { value, text: label })));
    select.value = query().remote || 'any';
    select.addEventListener('change', () => edited('remote', select.value === 'any' ? null : select.value));
    return select;
  }

  function sortSelect() {
    const select = h('select', { class: 'chip-select', 'aria-label': 'Order of the results' },
      ...SORTS.map(([value, label]) => h('option', { value, text: label })));
    select.value = query().sort || '';
    select.addEventListener('change', () => edited('sort', select.value || null));
    return select;
  }

  function filters() {
    const q = query();
    if (!q) return null;
    return [h('div', { class: 'chips search-chips' },
      ...listChips('keywords'),
      addWord('keywords', 'add a word…', '40'),
      ...listChips('categories'),
      addCategory(),
      ...listChips('industries'),
      addWord('industries', 'add a field…', '40'),
      ...scalarChips(q).map((c) => (c.key === 'near' ? nearChip() : chip(c.label, () => without(c.key)))),
      h('span', { class: 'chip-label', text: 'work style:' }),
      remoteSelect(),
      h('span', { class: 'chip-label', text: 'order:' }),
      sortSelect()),
      q.explanation ? h('p', { class: 'hint', text: `“${q.explanation}”` }) : null];
  }

  function source() {
    if (!result) return null;
    const where = result.llm && result.llm.host;
    const how = result.source === 'llm'
      ? `read by the LLM (${where || 'configured provider'})`
      : result.source === 'keywords'
        ? 'searched as keywords'
        : 'your filters';
    return h('p', { class: 'facts', text: `${whole.format(result.hits.length)} jobs · ${how}` });
  }

  function results() {
    if (!result) return null;
    const rows = result.hits.slice(0, SHOWN);
    if (!rows.length) {
      return h('p', { class: 'empty', text: 'Nothing matched. The notes above say what the search did; try dropping a filter or turning the LLM on.' });
    }
    return h('ul', { class: 'rows search-rows' }, ...rows.map((job) => {
      const pay = salary(job) || (job.salary_usd_annual ? `≈ $${whole.format(Math.round(job.salary_usd_annual))} / yr` : null);
      const meta = [job.company, job.location, job.remote_mode, pay,
        job.distance_km != null ? `${job.distance_km} km away` : null].filter(Boolean);
      const tags = [job.category, job.seniority, ...(job.skills || []).slice(0, 4)].filter(Boolean);
      return h('li', {},
        job.match_score != null ? h('span', { class: 'score', text: `${Math.round(job.match_score * 100)}%` }) : null,
        job.match_score != null ? ' ' : null,
        h('a', { href: job.url, target: '_blank', rel: 'noopener noreferrer', text: job.title }),
        h('div', { class: 'meta', text: meta.join(' · ') }),
        tags.length ? h('div', { class: 'chips' }, ...tags.map((t) => h('span', { class: 'chip', text: t }))) : null);
    }));
  }

  function render() {
    const parts = [form()];
    if (!result && !error && !busy) {
      parts.push(h('p', { class: 'hint', text: 'Describe a job in your own words. The search shows how it read you, and you can fix the filters it got wrong.' }));
    }
    if (error) parts.push(h('p', { class: 'error', role: 'alert', text: error }));
    if (busy) parts.push(h('p', { class: 'hint', text: 'Searching…' }));
    if (result) {
      parts.push(source());
      const notes = result.notes || [];
      if (notes.length) parts.push(h('ul', { class: 'notes' }, ...notes.map((n) => h('li', { text: n }))));
      parts.push(...filters());
      parts.push(results());
      const host = result.llm && result.llm.enabled && result.llm.host;
      parts.push(h('p', { class: 'hint', text: host
        ? `Your words are sent to ${host} to be read; the filters above are what came back.`
        : 'No LLM configured, so the words themselves are searched. Set [llm] enabled in config.toml to have them read.' }));
    }
    root.replaceChildren(...parts.filter(Boolean));
  }

  render();
  return { refresh: render };
}
