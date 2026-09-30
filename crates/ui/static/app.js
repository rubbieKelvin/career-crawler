// Wires the page together: totals, crawler controls, the domain graph, the live feed,
// domain details and the resource charts. All untrusted text (hosts, titles, URLs)
// goes into the DOM via textContent.

import { DomainGraph, PageGraph, PAGE_KINDS, STATUSES, pageColor, pageKind, statusColor } from '/static/graph.js';
import { LineChart } from '/static/charts.js';

const HISTORY_MS = 30 * 60 * 1000;
const GRAPH_NODES = 1500;
const FEED_MAX = 300;
const STATS_EVERY_MS = 5000;
const GRAPH_EVERY_MS = 15000;
/** Samples further apart than this (or from different runs) are not joined by a line. */
const SAMPLE_GAP_MS = 10000;
/** Replay fetches a graph snapshot at most this often while playing or scrubbing. */
const REPLAY_FETCH_MS = 350;
/** Live page-view refresh delay after a page of the drilled-into domain is fetched. */
const PAGE_VIEW_REFRESH_MS = 1500;

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

function renderTiles(items) {
  tiles.replaceChildren(...items.map(([label, value]) =>
    h('div', { class: 'tile' }, h('div', { class: 'label', text: label }), h('div', { class: 'value', text: value }))));
}

function renderStats(stats) {
  renderCrawler(stats.crawler);
  // While replaying, the tiles show totals at the replayed moment instead.
  if (replay.on) return;
  const c = stats.counts;
  const s = stats.metrics.latest;
  renderTiles([
    ['Companies', count(c.companies)],
    ['Open jobs', count(c.open_jobs)],
    ['Job boards', count(c.boards)],
    ['Domains', count(c.domains)],
    ['Pages', count(c.pages)],
    ['Queued', count(c.frontier_queued)],
    ['Data this run', stats.crawler.run_id && s && s.run_id === stats.crawler.run_id ? bytes(s.bytes_rx_wire) : '–'],
  ]);
}

function renderCrawler(status) {
  crawler = status;
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
const graphEmpty = document.getElementById('graph-empty');
const domainView = document.getElementById('domain-view');
const pageView = document.getElementById('page-view');
const graph = new DomainGraph(domainView, {
  onSelect: (host) => selectHost(host),
  onOpen: (host) => openPages(host),
});
const legend = document.getElementById('legend');

function legendKey(color, text) {
  return h('span', { class: 'key' }, h('span', { class: 'swatch', style: `background:${color}` }), text);
}

function renderLegend() {
  if (drillHost) {
    const nodes = pageData ? pageData.nodes : [];
    const counts = {};
    for (const n of nodes) counts[pageKind(n.kind)] = (counts[pageKind(n.kind)] || 0) + 1;
    const notes = ['The largest node is the home page'];
    if (pageData && pageData.hidden_pages) notes.push(`${whole.format(pageData.hidden_pages)} more pages not drawn`);
    if (replay.on) notes.push('Pages as of now, not the replayed moment');
    legend.replaceChildren(
      ...PAGE_KINDS.map((k) => legendKey(pageColor(k.key), `${k.label} (${whole.format(counts[k.key] || 0)})`)),
      h('span', { class: 'note', text: notes.join(' · ') }));
    return;
  }
  const counts = Object.fromEntries(STATUSES.map((s) => [s.key, 0]));
  for (const n of lastSnapshot.nodes) counts[n.status] = (counts[n.status] || 0) + 1;
  legend.replaceChildren(
    ...STATUSES.map((s) => legendKey(statusColor(s.key), `${s.label} (${whole.format(counts[s.key] || 0)})`)),
    h('span', { class: 'note', text: 'Size: pages + open jobs · double-click a domain to see its pages' }));
}

function showSnapshot(snapshot, { prune = false } = {}) {
  lastSnapshot = snapshot;
  graph.update(snapshot, { prune });
  graphEmpty.hidden = drillHost != null || !graph.empty;
  renderLegend();
}

let lastSnapshot = { nodes: [], edges: [] };
async function refreshGraph({ prune = false } = {}) {
  if (replay.on) return;
  try {
    showSnapshot(await api(`/api/graph?limit=${GRAPH_NODES}`), { prune });
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

// ---------- drill-down: one domain's pages ----------
const crumbRoot = document.getElementById('crumb-root');
const crumbHost = document.getElementById('crumb-host');
let pageGraph = null;
let pageData = null;
let drillHost = null;

async function openPages(host) {
  drillHost = host;
  domainView.hidden = true;
  pageView.hidden = false;
  graphEmpty.hidden = true;
  crumbRoot.disabled = false;
  crumbHost.hidden = false;
  document.getElementById('crumb-host-name').textContent = host;
  if (!pageGraph) pageGraph = new PageGraph(pageView, { onOpenSite: (site) => openPages(site) });
  else pageGraph.resize();
  pageData = null;
  renderLegend();
  // Re-render details too: the "explore" button hides while this domain is open.
  selectHost(host);
  await loadPages(host, { fit: true });
}

async function loadPages(host, { fit = false } = {}) {
  try {
    const resp = await fetch(`/api/domains/${encodeURIComponent(host)}/graph`);
    if (drillHost !== host) return;
    if (resp.status === 404) {
      pageData = { nodes: [], edges: [], hidden_pages: 0 };
      graphEmpty.textContent = `${host} hasn't been fetched yet.`;
      graphEmpty.hidden = false;
    } else {
      pageData = await resp.json();
      graphEmpty.hidden = true;
    }
    pageGraph.load(pageData);
    if (fit) setTimeout(() => drillHost === host && pageGraph.fit(), 1200);
    renderLegend();
  } catch (e) { console.warn(e); }
}

function closePages() {
  if (!drillHost) return;
  drillHost = null;
  pageData = null;
  pageView.hidden = true;
  domainView.hidden = false;
  crumbRoot.disabled = true;
  crumbHost.hidden = true;
  graphEmpty.textContent = 'Waiting for the first pages…';
  graphEmpty.hidden = !graph.empty;
  graph.resize();
  renderLegend();
  if (detailHost) selectHost(detailHost);
}

let pageRefreshTimer = null;
function schedulePageRefresh() {
  if (pageRefreshTimer || !drillHost) return;
  const host = drillHost;
  pageRefreshTimer = setTimeout(() => { pageRefreshTimer = null; if (drillHost === host) loadPages(host); }, PAGE_VIEW_REFRESH_MS);
}

crumbRoot.addEventListener('click', closePages);
document.addEventListener('keydown', (e) => {
  const typing = e.target instanceof Element && e.target.closest('input, select, textarea');
  if (e.key === 'Escape' && drillHost && !typing) closePages();
});

document.getElementById('find-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const host = document.getElementById('find').value.trim().toLowerCase();
  if (!host) return;
  closePages();
  selectHost(host);
});
document.getElementById('btn-fit').addEventListener('click', () => (drillHost ? pageGraph : graph).fit());
function themeChanged() {
  graph.applyTheme();
  if (pageGraph) pageGraph.applyTheme();
  renderLegend();
  if (replay.on) renderActivity();
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
  jobs_found: 'jobs', jobs_enriched: 'jobs', careers_found: 'careers', domain_classified: 'classify', fetch_failed: 'errors',
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
    case 'jobs_enriched':
      return ['Enriched', `${whole.format(e.total)} jobs${e.llm ? ` · ${whole.format(e.llm)} with the LLM` : ''}`];
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
    drillHost === dom.host
      ? null
      : h('div', { class: 'explore' },
          h('button', { class: 'btn', type: 'button', text: 'explore its pages →', onclick: () => openPages(dom.host) })),
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
  detail.replaceChildren(...sections.filter(Boolean));
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

// ---------- history replay ----------
const replayBtn = document.getElementById('btn-replay');
const timeline = document.getElementById('timeline');
const scrubber = document.getElementById('scrubber');
const playBtn = document.getElementById('btn-play');
const speedSelect = document.getElementById('replay-speed');
const whenEl = document.getElementById('replay-when');
const activityEl = document.getElementById('activity');
const replay = { on: false, t: 0, history: null, playing: false, lastFrame: 0, busy: false, pendingT: null, lastFetch: 0 };
const replayTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });

async function enterReplay() {
  try { replay.history = await api('/api/history?buckets=160'); } catch (e) { console.warn(e); return; }
  if (replay.history.start == null) return;
  replay.on = true;
  replayBtn.setAttribute('aria-pressed', 'true');
  timeline.hidden = false;
  scrubber.min = String(replay.history.start);
  scrubber.max = String(replay.history.end);
  renderActivity();
  // Start from the beginning so "play" shows the web growing.
  setReplayTime(replay.history.start, { force: true });
}

function exitReplay() {
  if (!replay.on) return;
  stopPlaying();
  replay.on = false;
  replayBtn.setAttribute('aria-pressed', 'false');
  timeline.hidden = true;
  refreshGraph({ prune: true });
  refreshStats();
  reloadFeed();
  if (drillHost) loadPages(drillHost);
}

function setReplayTime(t, { force = false } = {}) {
  const { start, end } = replay.history;
  replay.t = Math.min(end, Math.max(start, t));
  scrubber.value = String(Math.round(replay.t));
  const bucket = activityAt(replay.t);
  whenEl.textContent = `${replayTime.format(replay.t)}${bucket ? ` · ${whole.format(bucket.pages)} pages fetched around then` : ''}`;
  requestReplaySnapshot(force);
}

function activityAt(t) {
  const { bucket_ms: size, activity } = replay.history;
  return activity.find((b) => t >= b.t && t < b.t + size) || null;
}

/** At most one snapshot request in flight; the latest requested time wins. */
async function requestReplaySnapshot(force) {
  const now = performance.now();
  if (replay.busy || (!force && now - replay.lastFetch < REPLAY_FETCH_MS)) {
    replay.pendingT = replay.t;
    if (!replay.busy) setTimeout(() => replay.pendingT != null && requestReplaySnapshot(true), REPLAY_FETCH_MS);
    return;
  }
  replay.busy = true;
  replay.pendingT = null;
  replay.lastFetch = now;
  const t = Math.round(replay.t);
  try {
    const [snapshot, events] = await Promise.all([
      api(`/api/graph?limit=${GRAPH_NODES}&at=${t}`),
      api(`/api/events?before=${t}&limit=150`),
    ]);
    if (!replay.on) return;
    showSnapshot(snapshot, { prune: true });
    const c = snapshot.counts;
    renderTiles([
      ['Companies', count(c.companies)],
      ['Open jobs', count(c.open_jobs)],
      ['Job boards', count(c.boards)],
      ['Domains', count(c.domains)],
      ['Pages', count(c.pages)],
    ]);
    feed.replaceChildren();
    events.forEach((m) => addFeed(m.id, m.ts, m.event));
  } catch (e) {
    console.warn(e);
  } finally {
    replay.busy = false;
    if (replay.on && replay.pendingT != null) requestReplaySnapshot(true);
  }
}

function play() {
  if (replay.t >= replay.history.end) setReplayTime(replay.history.start, { force: true });
  replay.playing = true;
  replay.lastFrame = performance.now();
  playBtn.textContent = 'pause';
  requestAnimationFrame(playFrame);
}

/** Playback jumps over time when no crawler was running: straight to the next run. */
function skipIdle(t) {
  const runs = replay.history.runs;
  const inRun = runs.some((r) => t >= r.started_at && t <= (r.stopped_at ?? replay.history.end));
  if (inRun) return t;
  const next = runs.find((r) => r.started_at > t);
  return next ? next.started_at : t;
}

function stopPlaying() {
  replay.playing = false;
  playBtn.textContent = 'play';
}

function playFrame(now) {
  if (!replay.playing || !replay.on) return;
  const dt = now - replay.lastFrame;
  replay.lastFrame = now;
  setReplayTime(skipIdle(replay.t + dt * Number(speedSelect.value)));
  if (replay.t >= replay.history.end) {
    stopPlaying();
    return;
  }
  requestAnimationFrame(playFrame);
}

/** The activity strip above the scrubber: pages fetched per bucket, and crawler runs. */
function renderActivity() {
  const { start, end, bucket_ms: size, activity, runs } = replay.history;
  const width = activityEl.clientWidth || 600;
  const height = 28;
  const svg = 'http://www.w3.org/2000/svg';
  activityEl.setAttribute('viewBox', `0 0 ${width} ${height}`);
  const x = (t) => ((t - start) / Math.max(1, end - start)) * width;
  const max = Math.max(1, ...activity.map((b) => b.pages));
  const barWidth = Math.max(1, (size / Math.max(1, end - start)) * width - 1);
  const marks = [];
  for (const b of activity) {
    const hgt = Math.max(1, (b.pages / max) * (height - 6));
    const rect = document.createElementNS(svg, 'rect');
    rect.setAttribute('x', x(b.t).toFixed(1));
    rect.setAttribute('y', (height - 4 - hgt).toFixed(1));
    rect.setAttribute('width', barWidth.toFixed(1));
    rect.setAttribute('height', hgt.toFixed(1));
    rect.setAttribute('fill', 'var(--muted)');
    rect.setAttribute('fill-opacity', '0.55');
    marks.push(rect);
  }
  for (const run of runs) {
    const line = document.createElementNS(svg, 'line');
    line.setAttribute('x1', x(run.started_at).toFixed(1));
    line.setAttribute('x2', x(run.stopped_at ?? end).toFixed(1));
    line.setAttribute('y1', String(height - 1));
    line.setAttribute('y2', String(height - 1));
    line.setAttribute('stroke', 'var(--mark)');
    line.setAttribute('stroke-width', '2');
    marks.push(line);
  }
  activityEl.replaceChildren(...marks);
}

async function reloadFeed() {
  try {
    const history = await api('/api/events?limit=150');
    feed.replaceChildren();
    history.forEach((m) => addFeed(m.id, m.ts, m.event));
  } catch (e) { console.warn(e); }
}

replayBtn.addEventListener('click', () => (replay.on ? exitReplay() : enterReplay()));
document.getElementById('btn-live').addEventListener('click', exitReplay);
playBtn.addEventListener('click', () => (replay.playing ? stopPlaying() : play()));
scrubber.addEventListener('input', () => {
  stopPlaying();
  setReplayTime(Number(scrubber.value));
});
new ResizeObserver(() => replay.on && renderActivity()).observe(activityEl);

// ---------- live connection ----------
let statsTimer = null;
function throttledStats() {
  if (statsTimer) return;
  statsTimer = setTimeout(() => { statsTimer = null; refreshStats(); }, 1000);
}

function onEvent(msg) {
  const e = msg.event;
  if (['crawler_started', 'crawler_stopped', 'control_applied'].includes(e.kind)) refreshStats();
  // A replay shows the past: live events wait until "back to live".
  if (replay.on) return;
  addFeed(msg.id, msg.ts, e);
  const host = e.domain;
  if (host && host === drillHost && ['page_fetched', 'fetch_failed', 'careers_found', 'jobs_found'].includes(e.kind)) {
    schedulePageRefresh();
  }
  if (host && !graph.has(host)) scheduleGraphRefresh();
  switch (e.kind) {
    case 'page_fetched': graph.pulse(host); break;
    case 'domain_classified': graph.setStatus(host, e.status); scheduleGraphRefresh(); break;
    case 'jobs_found': case 'careers_found':
      if (host) graph.pulse(host);
      scheduleGraphRefresh();
      if (host && host === detailHost) selectHost(host);
      break;
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
  await reloadFeed();
  connect();
  setInterval(refreshStats, STATS_EVERY_MS);
  setInterval(refreshGraph, GRAPH_EVERY_MS);
}
start();
