// Companies: every domain the crawler has seen, filterable and sortable. Filters and paging
// live in the URL so a view can be shared. All untrusted text goes in via textContent.

import { api, count, dateTime, domainHref, h, link, whole } from '/static/common.js';
import { STATUSES } from '/static/graph.js';
import { initShell } from '/static/shell.js';

const PAGE_SIZE = 50;
const STATUS_LABELS = Object.fromEntries(STATUSES.map((s) => [s.key, s.label]));
const COLUMNS = [
  { key: 'host', label: 'Company', sort: 'host' },
  { key: 'status', label: 'Status' },
  { key: 'score', label: 'Score', sort: 'score', num: true },
  { key: 'jobs', label: 'Open jobs', sort: 'jobs', num: true },
  { key: 'pages', label: 'Pages', num: true },
  { key: 'careers', label: 'Careers' },
  { key: 'last_crawled', label: 'Last crawled', sort: 'last_crawled', num: true },
];

const shell = initShell('companies');
const root = document.getElementById('companies-root');

const state = { q: '', status: 'company', has_jobs: '', has_careers: '', ats: '', sort: 'last_crawled', desc: true, offset: 0 };
const params = new URLSearchParams(location.search);
for (const key of Object.keys(state)) {
  if (!params.has(key)) continue;
  state[key] = key === 'desc' ? params.get(key) !== 'false' : key === 'offset' ? Number(params.get(key)) || 0 : params.get(key);
}

function queryString() {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(state)) if (v !== '' && v != null) p.set(k, String(v));
  return p.toString();
}

function select(name, options) {
  const el = h('select', { class: 'chip-select', 'aria-label': name });
  for (const [value, label] of options) el.append(h('option', { value, text: label }));
  el.value = state[name];
  el.addEventListener('change', () => { state[name] = el.value; state.offset = 0; load(); });
  return el;
}

const search = h('input', { type: 'search', placeholder: 'Filter by name or domain', value: state.q, 'aria-label': 'Filter by name or domain' });
let searchTimer = null;
search.addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => { state.q = search.value; state.offset = 0; load(); }, 250);
});
const statusSel = select('status', [['all', 'Any status'], ...STATUSES.map((s) => [s.key, s.label])]);
const jobsSel = select('has_jobs', [['', 'Jobs: any'], ['true', 'Has open jobs'], ['false', 'No open jobs']]);
const careersSel = select('has_careers', [['', 'Careers: any'], ['true', 'Careers page found'], ['false', 'No careers page']]);
const atsSel = select('ats', [['', 'Job board: any']]);

const summary = h('p', { class: 'hint' });
const tableWrap = h('div', { class: 'table-wrap' });
const pager = h('div', { class: 'pager' });
root.append(
  h('div', { class: 'search-form' }, search),
  h('div', { class: 'chips search-chips' }, statusSel, jobsSel, careersSel, atsSel),
  summary, tableWrap, pager);

function cell(col, row) {
  switch (col.key) {
    case 'host': {
      const a = h('a', { href: domainHref(row.host), text: row.name || row.host });
      return h('td', {}, a, row.name ? h('div', { class: 'sub', text: row.host }) : null);
    }
    case 'status': return h('td', { text: STATUS_LABELS[row.status] || row.status });
    case 'score': return h('td', { text: row.company_score == null ? '–' : row.company_score.toFixed(2) });
    case 'jobs': return h('td', { text: count(row.open_jobs) });
    case 'pages': return h('td', { text: count(row.pages) });
    case 'careers': return h('td', {}, row.careers_url ? link(row.careers_url, row.ats || 'careers') : '–');
    default: return h('td', { text: row.last_crawled ? dateTime.format(row.last_crawled) : 'never' });
  }
}

function render(data) {
  const head = COLUMNS.map((col) => {
    const th = h('th', { class: col.num ? 'num' : '' });
    if (!col.sort) { th.textContent = col.label; return th; }
    const active = state.sort === col.sort;
    const btn = h('button', { class: 'sort', text: col.label + (active ? (state.desc ? ' ↓' : ' ↑') : '') });
    btn.addEventListener('click', () => {
      state.desc = active ? !state.desc : col.sort !== 'host';
      state.sort = col.sort;
      state.offset = 0;
      load();
    });
    th.setAttribute('aria-sort', active ? (state.desc ? 'descending' : 'ascending') : 'none');
    th.append(btn);
    return th;
  });
  const rows = data.rows.map((row) => h('tr', {}, ...COLUMNS.map((col) => {
    const td = cell(col, row);
    if (col.num) td.className = 'num';
    return td;
  })));
  tableWrap.replaceChildren(rows.length
    ? h('table', { class: 'samples companies-table' }, h('thead', {}, h('tr', {}, ...head)), h('tbody', {}, ...rows))
    : h('p', { class: 'empty', text: 'No companies match these filters.' }));

  const from = data.total ? state.offset + 1 : 0;
  const to = state.offset + data.rows.length;
  summary.textContent = `${whole.format(data.total)} ${data.total === 1 ? 'domain' : 'domains'}` + (data.total ? ` · showing ${whole.format(from)}–${whole.format(to)}` : '');
  const prev = h('button', { class: 'btn', text: '← Previous' });
  const next = h('button', { class: 'btn', text: 'Next →' });
  prev.disabled = state.offset <= 0;
  next.disabled = to >= data.total;
  prev.addEventListener('click', () => { state.offset = Math.max(0, state.offset - PAGE_SIZE); load(); });
  next.addEventListener('click', () => { state.offset += PAGE_SIZE; load(); });
  pager.replaceChildren(prev, next);

  if (atsSel.options.length !== data.vendors.length + 1) {
    atsSel.replaceChildren(h('option', { value: '', text: 'Job board: any' }),
      ...data.vendors.map((v) => h('option', { value: v, text: v })));
    atsSel.value = state.ats;
  }
}

let loading = false;
let again = false;
async function load() {
  // Single-flight, with one trailing reload so the last filter change always wins.
  if (loading) { again = true; return; }
  loading = true;
  try {
    history.replaceState(null, '', `?${queryString()}`);
    render(await api(`/api/companies?${queryString()}&limit=${PAGE_SIZE}`));
  } catch (e) {
    console.warn(e);
    summary.textContent = 'Could not load companies.';
  } finally {
    loading = false;
    if (again) { again = false; load(); }
  }
}

shell.on('resync', () => load());
load();
