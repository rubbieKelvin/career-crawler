// Wires the page together: totals, crawler controls, the domain graph, the live feed,
// domain details and the resource charts. All untrusted text (hosts, titles, URLs)
// goes into the DOM via textContent.

import { DomainGraph, STATUSES, statusColor } from '/static/graph.js';
import { LineChart } from '/static/charts.js';

const HISTORY_MS = 30 * 60 * 1000;
const GRAPH_NODES = 1500;
const FEED_MAX = 300;
const STATS_EVERY_MS = 5000;
const GRAPH_EVERY_MS = 15000;
/** Samples further apart than this (or from different runs) are not joined by a line. */
const SAMPLE_GAP_MS = 10000;

// ---------- formatting ----------
const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
const whole = new Intl.NumberFormat();
const clock = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const dateTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

/** Decimal units (1 kB = 1000 B), so round axis ticks read as round numbers. */
function bytes(n) {
  if (n == null) return '–';
  const units = ['B', 'kB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) { n /= 1000; i++; }
  return `${n >= 100 || i === 0 ? Math.round(n) : Number(n.toFixed(1))} ${units[i]}`;
}
const count = (n) => (n == null ? '–' : n < 10000 ? whole.format(n) : compact.format(n));

function salary(job) {
  if (job.salary_min == null && job.salary_max == null) return null;
  const f = (v) => compact.format(v);
  const range = job.salary_min != null && job.salary_max != null && job.salary_min !== job.salary_max
    ? `${f(job.salary_min)}–${f(job.salary_max)}`
    : f(job.salary_min ?? job.salary_max);
  return `${job.salary_currency || ''} ${range}${job.salary_period ? ` / ${job.salary_period}` : ''}`.trim();
}

function h(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === 'text') node.textContent = v;
    else if (k === 'class') node.className = v;
    else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v);
  }
  for (const c of children) if (c != null) node.append(c);
  return node;
}

async function api(path, options) {
  const resp = await fetch(path, options);
  if (!resp.ok) throw new Error(`${path}: ${resp.status}`);
  return resp.json();
}

// ---------- totals and crawler status ----------
const tiles = document.getElementById('tiles');
const statePill = document.getElementById('crawler-state');
const pauseBtn = document.getElementById('btn-pause');
const stopBtn = document.getElementById('btn-stop');
let crawler = { running: false, paused: false };

function renderStats(stats) {
  const c = stats.counts;
  const s = stats.metrics.latest;
  const items = [
    ['Companies', count(c.companies)],
    ['Open jobs', count(c.open_jobs)],
    ['Job boards', count(c.boards)],
    ['Domains', count(c.domains)],
    ['Pages', count(c.pages)],
    ['Queued', count(c.frontier_queued)],
    ['Data this run', stats.crawler.run_id && s && s.run_id === stats.crawler.run_id ? bytes(s.bytes_rx_wire) : '–'],
  ];
  tiles.replaceChildren(...items.map(([label, value]) =>
    h('div', { class: 'tile' }, h('div', { class: 'label', text: label }), h('div', { class: 'value', text: value }))));

  crawler = stats.crawler;
  const state = crawler.paused ? 'paused' : crawler.running ? 'running' : 'stopped';
  statePill.dataset.state = state;
  const label = state === 'paused' ? 'Paused' : state === 'running' ? 'Crawling' : 'Idle';
  statePill.querySelector('.text').textContent = label;
  statePill.title = label;
  pauseBtn.disabled = !crawler.running;
  pauseBtn.textContent = crawler.paused ? 'Resume' : 'Pause';
  stopBtn.disabled = !crawler.running;
}

async function refreshStats() {
  try { renderStats(await api('/api/stats')); } catch (e) { console.warn(e); }
}

pauseBtn.addEventListener('click', async () => {
  await api(`/api/control/${crawler.paused ? 'resume' : 'pause'}`, { method: 'POST' });
  pauseBtn.disabled = true;
  setTimeout(refreshStats, 800);
});
stopBtn.addEventListener('click', async () => {
  if (!confirm('Stop the crawler? In-flight pages finish first.')) return;
  await api('/api/control/stop', { method: 'POST' });
  stopBtn.disabled = true;
  setTimeout(refreshStats, 800);
});

// ---------- graph ----------
const graphEl = document.getElementById('graph');
const graphEmpty = document.getElementById('graph-empty');
const graph = new DomainGraph(graphEl, { onSelect: (host) => selectHost(host) });
const legend = document.getElementById('legend');

function renderLegend(nodes) {
  const counts = Object.fromEntries(STATUSES.map((s) => [s.key, 0]));
  for (const n of nodes) counts[n.status] = (counts[n.status] || 0) + 1;
  legend.replaceChildren(
    ...STATUSES.map((s) => h('span', { class: 'key' },
      h('span', { class: 'swatch', style: `background:${statusColor(s.key)}` }),
      `${s.label} (${whole.format(counts[s.key] || 0)})`)),
    h('span', { class: 'note', text: 'Size: pages + open jobs' }));
}

let lastSnapshot = { nodes: [] };
async function refreshGraph() {
  try {
    lastSnapshot = await api(`/api/graph?limit=${GRAPH_NODES}`);
    graph.update(lastSnapshot);
    graphEmpty.hidden = !graph.empty;
    renderLegend(lastSnapshot.nodes);
    const datalist = document.getElementById('hosts');
    datalist.replaceChildren(...lastSnapshot.nodes.filter((n) => n.status !== 'discovered').slice(0, 500)
      .map((n) => h('option', { value: n.host })));
  } catch (e) { console.warn(e); }
}

let graphRefreshTimer = null;
function scheduleGraphRefresh() {
  if (graphRefreshTimer) return;
  graphRefreshTimer = setTimeout(() => { graphRefreshTimer = null; refreshGraph(); }, 2000);
}

document.getElementById('find-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const host = document.getElementById('find').value.trim().toLowerCase();
  if (host) selectHost(host);
});
document.getElementById('btn-fit').addEventListener('click', () => graph.fit());
function themeChanged() {
  graph.applyTheme();
  renderLegend(lastSnapshot.nodes);
  charts.forEach((c) => c.chart.render());
}
const darkQuery = matchMedia('(prefers-color-scheme: dark)');
const isDark = () => {
  const t = document.documentElement.dataset.theme;
  return t ? t === 'dark' : darkQuery.matches;
};
const themeBtn = document.getElementById('btn-theme');
const syncThemeBtn = () => { themeBtn.textContent = isDark() ? 'light' : 'dark'; };
syncThemeBtn();
themeBtn.addEventListener('click', () => {
  const next = isDark() ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('theme', next); } catch (e) { /* storage blocked: choice lasts this page load */ }
  syncThemeBtn();
  themeChanged();
});
darkQuery.addEventListener('change', () => { syncThemeBtn(); themeChanged(); });

// ---------- tabs ----------
const tabs = { feed: document.getElementById('tab-feed'), detail: document.getElementById('tab-detail') };
const panels = { feed: document.getElementById('panel-feed'), detail: document.getElementById('panel-detail') };
const feedFilter = document.getElementById('feed-filter');
function showTab(name) {
  for (const key of Object.keys(tabs)) {
    tabs[key].setAttribute('aria-selected', String(key === name));
    panels[key].hidden = key !== name;
  }
  feedFilter.hidden = name !== 'feed';
}
tabs.feed.addEventListener('click', () => showTab('feed'));
tabs.detail.addEventListener('click', () => showTab('detail'));

// ---------- live feed ----------
const feed = document.getElementById('feed');
const CATEGORY = {
  jobs_found: 'jobs', careers_found: 'careers', domain_classified: 'classify', fetch_failed: 'errors',
  crawler_started: 'crawler', crawler_stopped: 'crawler', seeds_loaded: 'crawler', control_applied: 'crawler',
};
const statusLabel = (key) => (STATUSES.find((s) => s.key === key) || { label: key }).label;

function hostButton(host) {
  return host ? h('button', { class: 'host', type: 'button', text: host, onclick: () => selectHost(host) }) : null;
}
function shortUrl(url) {
  try { const u = new URL(url); return u.host + (u.pathname === '/' ? '' : u.pathname); } catch { return url; }
}

/** Human summary of an event: [label, ...content nodes]. */
function describe(e) {
  switch (e.kind) {
    case 'page_fetched':
      return ['Fetched', shortUrl(e.url), ` · ${e.links} links, ${e.enqueued} queued${e.duplicate ? ' · duplicate' : ''}`];
    case 'fetch_failed':
      return ['Not fetched', shortUrl(e.url), ` · ${e.reason.replaceAll('_', ' ')}${e.will_retry ? ' · will retry' : ''}`];
    case 'domain_classified':
      return ['Classified', hostButton(e.domain), ` → ${statusLabel(e.status)} (${Math.round(e.score * 100)}%)`];
    case 'careers_found':
      return ['Careers page', hostButton(e.domain), ` · ${e.ats ? `${e.ats} board` : shortUrl(e.url)}`];
    case 'jobs_found':
      return ['Jobs', e.domain ? hostButton(e.domain) : (e.board || shortUrl(e.url)),
        ` · ${whole.format(e.total)} open, ${whole.format(e.new)} new${e.closed ? `, ${e.closed} closed` : ''}`];
    case 'crawler_started': return ['Crawler', 'started'];
    case 'crawler_stopped': return ['Crawler', `stopped: ${e.reason.replaceAll('_', ' ')}`];
    case 'seeds_loaded': return ['Seeds', `${e.parsed} loaded, ${e.enqueued} new`];
    case 'control_applied': return ['Control', `${e.command} (${e.source})`];
    default: return [e.kind, JSON.stringify(e)];
  }
}

function addFeed(id, ts, event) {
  const [what, ...content] = describe(event);
  const item = h('li', { 'data-category': CATEGORY[event.kind] || 'crawl' },
    h('span', { class: 'time', text: clock.format(ts) }),
    h('div', {}, h('div', { class: 'what', text: what }), h('div', { class: 'text' }, ...content)));
  applyFilter(item);
  feed.prepend(item);
  while (feed.children.length > FEED_MAX) feed.lastChild.remove();
}
function applyFilter(item) {
  const f = feedFilter.value;
  item.hidden = f !== 'all' && item.dataset.category !== f;
}
feedFilter.addEventListener('change', () => [...feed.children].forEach(applyFilter));

// ---------- domain details ----------
const detail = panels.detail;
let detailHost = null;

async function selectHost(host) {
  detailHost = host;
  graph.focus(host) || graph.select(host);
  showTab('detail');
  detail.replaceChildren(h('p', { class: 'empty', text: `Loading ${host}…` }));
  try {
    const resp = await fetch(`/api/domains/${encodeURIComponent(host)}`);
    if (detailHost !== host) return;
    if (resp.status === 404) {
      detail.replaceChildren(h('p', { class: 'empty', text: `${host} hasn't been seen by the crawler.` }));
      return;
    }
    renderDetail(await resp.json());
  } catch (e) {
    detail.replaceChildren(h('p', { class: 'empty', text: `Couldn't load ${host}.` }));
  }
}

function link(url, text) {
  return h('a', { href: url, target: '_blank', rel: 'noopener noreferrer', text: text || shortUrl(url) });
}

function renderDetail(d) {
  const dom = d.domain;
  const signals = (dom.score_reasons && dom.score_reasons.signals) || [];
  const facts = [
    ['Careers page', dom.careers_url ? link(dom.careers_url) : 'not found yet'],
    ['Job board', dom.ats ? `${dom.ats} / ${dom.ats_token}` : '–'],
    ['Pages', whole.format(d.page_count)],
    ['Open jobs', whole.format(d.open_jobs)],
    ['Links', `${whole.format(d.inbound)} in · ${whole.format(d.outbound)} out`],
    ['First seen', dateTime.format(dom.first_seen)],
    ['Last crawled', dom.last_crawled ? dateTime.format(dom.last_crawled) : 'never'],
  ];
  const sections = [
    h('h3', { text: dom.name || dom.host }),
    h('div', { class: 'host-line' }, link(`https://${dom.host}/`, dom.host)),
    h('div', { class: 'status-key' },
      h('span', { class: 'swatch', style: `background:${statusColor(dom.status)}` }),
      `${statusLabel(dom.status)}${dom.company_score != null ? ` · company score ${Math.round(dom.company_score * 100)}%` : ''}`),
    h('section', {}, h('dl', { class: 'facts' }, ...facts.flatMap(([k, v]) => [h('dt', { text: k }), h('dd', {}, v)]))),
  ];
  if (signals.length) {
    sections.push(h('section', {}, h('h4', { text: 'Why' }),
      h('div', { class: 'chips' }, ...signals.map((s) => h('span', { class: 'chip', text: s.replaceAll('_', ' ') })))));
  }
  if (d.boards.length) {
    sections.push(h('section', {}, h('h4', { text: 'Job boards' }), h('ul', { class: 'rows' },
      ...d.boards.map((b) => h('li', {}, b.key, h('div', { class: 'meta', text:
        `${b.job_count != null ? `${whole.format(b.job_count)} jobs · ` : ''}${b.last_status || 'not fetched yet'}` }))))));
  }
  sections.push(h('section', {}, h('h4', { text: `Open jobs (${whole.format(d.open_jobs)})` }),
    d.jobs.length
      ? h('ul', { class: 'rows' }, ...d.jobs.map((j) => h('li', {}, link(j.url, j.title),
          h('div', { class: 'meta', text: [j.location, j.remote_mode, j.department, salary(j)].filter(Boolean).join(' · ') }))))
      : h('p', { class: 'empty', text: 'No jobs found yet.' })));
  sections.push(h('section', {}, h('h4', { text: d.page_count > d.pages.length ? `Pages (first ${d.pages.length})` : 'Pages' }),
    h('ul', { class: 'rows' }, ...d.pages.map((p) => h('li', {}, link(p.url),
      h('div', { class: 'meta', text: [p.kind, p.http_status, p.error].filter((x) => x != null).join(' · ') }))))));
  detail.replaceChildren(...sections);
}

// ---------- metrics ----------
const chartsEl = document.getElementById('charts');
let samples = [];

/** Derived per-sample series; rates are null across a run boundary. */
const SERIES = [
  { title: 'Download rate', format: (v) => `${bytes(v)}/s`,
    value: (p, s) => (p && p.run_id === s.run_id && s.ts > p.ts ? Math.max(0, (s.bytes_rx_wire - p.bytes_rx_wire) / ((s.ts - p.ts) / 1000)) : null) },
  { title: 'Pages per minute', format: (v) => whole.format(Math.round(v)),
    value: (p, s) => (p && p.run_id === s.run_id && s.ts > p.ts ? Math.max(0, (s.pages - p.pages) / ((s.ts - p.ts) / 60000)) : null) },
  { title: 'CPU', format: (v) => `${Math.round(v)}%`, value: (_, s) => s.cpu_pct },
  { title: 'Memory (RSS)', format: bytes, value: (_, s) => s.rss_bytes },
];
const charts = SERIES.map((spec) => ({ spec, chart: new LineChart(chartsEl, spec) }));

function renderCharts() {
  const cutoff = Date.now() - HISTORY_MS;
  samples = samples.filter((s) => s.ts >= cutoff);
  for (const { spec, chart } of charts) {
    const points = [];
    samples.forEach((s, i) => {
      const prev = samples[i - 1];
      // A null point breaks the line: the crawler wasn't running in between.
      if (prev && (prev.run_id !== s.run_id || s.ts - prev.ts > SAMPLE_GAP_MS)) points.push({ t: s.ts - 1, v: null });
      points.push({ t: s.ts, v: spec.value(prev, s) ?? null });
    });
    chart.setData(points);
  }
  if (!tableWrap.hidden) renderTable();
}

const tableWrap = document.getElementById('samples-table');
const tableBtn = document.getElementById('btn-table');
tableBtn.addEventListener('click', () => {
  tableWrap.hidden = !tableWrap.hidden;
  tableBtn.setAttribute('aria-expanded', String(!tableWrap.hidden));
  tableBtn.textContent = tableWrap.hidden ? 'Show table' : 'Hide table';
  if (!tableWrap.hidden) renderTable();
});
function renderTable() {
  const recent = samples.slice(-30).reverse();
  const head = h('tr', {}, h('th', { text: 'Time' }), ...SERIES.map((s) => h('th', { text: s.title })));
  const rows = recent.map((s) => {
    const i = samples.indexOf(s);
    return h('tr', {}, h('td', { text: clock.format(s.ts) }),
      ...SERIES.map((spec) => { const v = spec.value(samples[i - 1], s); return h('td', { text: v == null ? '–' : spec.format(v) }); }));
  });
  tableWrap.replaceChildren(h('table', { class: 'samples' }, h('thead', {}, head), h('tbody', {}, ...rows)));
}

async function loadHistory() {
  try {
    samples = await api(`/api/metrics/history?from=${Date.now() - HISTORY_MS}`);
    renderCharts();
  } catch (e) { console.warn(e); }
}

// ---------- live connection ----------
let statsTimer = null;
function throttledStats() {
  if (statsTimer) return;
  statsTimer = setTimeout(() => { statsTimer = null; refreshStats(); }, 1000);
}

function onEvent(msg) {
  const e = msg.event;
  addFeed(msg.id, msg.ts, e);
  const host = e.domain;
  if (host && !graph.has(host)) scheduleGraphRefresh();
  switch (e.kind) {
    case 'page_fetched': graph.pulse(host); break;
    case 'domain_classified': graph.setStatus(host, e.status); scheduleGraphRefresh(); break;
    case 'jobs_found': case 'careers_found':
      if (host) graph.pulse(host);
      scheduleGraphRefresh();
      if (host && host === detailHost) selectHost(host);
      break;
    case 'crawler_started': case 'crawler_stopped': case 'control_applied': refreshStats(); break;
  }
}

function connect(delay = 1000) {
  const ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
  ws.onopen = () => { delay = 1000; refreshStats(); refreshGraph(); };
  ws.onmessage = (raw) => {
    const msg = JSON.parse(raw.data);
    if (msg.type === 'event') onEvent(msg);
    else if (msg.type === 'metrics') {
      samples.push(msg.sample);
      renderCharts();
      throttledStats();
    } else if (msg.type === 'lagged') {
      refreshStats();
      refreshGraph();
    }
  };
  ws.onclose = () => setTimeout(() => connect(Math.min(delay * 2, 15000)), delay);
}

async function start() {
  await Promise.all([refreshStats(), refreshGraph(), loadHistory()]);
  try {
    const history = await api('/api/events?limit=150');
    history.forEach((m) => addFeed(m.id, m.ts, m.event));
  } catch (e) { console.warn(e); }
  connect();
  setInterval(refreshStats, STATS_EVERY_MS);
  setInterval(refreshGraph, GRAPH_EVERY_MS);
}
start();
