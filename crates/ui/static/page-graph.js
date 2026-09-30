// Graph: the web of companies (live or replayed from history), a domain's page-level
// drill-down, and the details of the selected domain. `?host=` opens a domain's details and
// `?host=…&view=pages` drills into its pages. All untrusted text goes in via textContent.

import { DomainGraph, PageGraph, PAGE_KINDS, STATUSES, pageColor, pageKind, statusColor } from '/static/graph.js';
import { api, count, dateTime, h, link, renderTiles, salary, statTiles, whole } from '/static/common.js';
import { createFeed } from '/static/feed.js';
import { initShell } from '/static/shell.js';

const GRAPH_NODES = 1500;
const GRAPH_EVERY_MS = 15000;
/** Replay fetches a graph snapshot at most this often while playing or scrubbing. */
const REPLAY_FETCH_MS = 350;
/** Live page-view refresh delay after a page of the drilled-into domain is fetched. */
const PAGE_VIEW_REFRESH_MS = 1500;

const shell = initShell('graph');
const statusLabel = (key) => (STATUSES.find((s) => s.key === key) || { label: key }).label;

// ---------- tiles ----------
const tiles = document.getElementById('tiles');
shell.on('stats', (stats) => {
  // While replaying, the tiles show totals at the replayed moment instead.
  if (!replay.on) renderTiles(tiles, statTiles(stats));
});

// ---------- tabs: details / live feed ----------
const tabs = { detail: document.getElementById('tab-detail'), feed: document.getElementById('tab-feed') };
const panels = { detail: document.getElementById('panel-detail'), feed: document.getElementById('panel-feed') };
const feedFilter = document.getElementById('feed-filter');
function showTab(name) {
  for (const key of Object.keys(tabs)) {
    tabs[key].setAttribute('aria-selected', String(key === name));
    panels[key].hidden = key !== name;
  }
  feedFilter.hidden = name !== 'feed';
}
tabs.detail.addEventListener('click', () => showTab('detail'));
tabs.feed.addEventListener('click', () => showTab('feed'));
showTab('detail');

const feed = createFeed({ list: document.getElementById('feed'), filter: feedFilter, onHost: (host) => selectHost(host) });
async function reloadFeed() {
  try { feed.reset(await api('/api/events?limit=150')); } catch (e) { console.warn(e); }
}

// ---------- graph ----------
const graphEmpty = document.getElementById('graph-empty');
const domainView = document.getElementById('domain-view');
const pageView = document.getElementById('page-view');
const graph = new DomainGraph(domainView, {
  onSelect: (host) => selectHost(host),
  onOpen: (host) => openPages(host),
});
const legend = document.getElementById('legend');
let lastSnapshot = { nodes: [], edges: [] };

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

/** Keeps the address bar shareable without adding history entries. */
function syncUrl() {
  const params = new URLSearchParams();
  if (detailHost) params.set('host', detailHost);
  if (drillHost) params.set('view', 'pages');
  const query = params.toString();
  history.replaceState(null, '', query ? `?${query}` : location.pathname);
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
  else syncUrl();
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
shell.on('theme', () => {
  graph.applyTheme();
  if (pageGraph) pageGraph.applyTheme();
  renderLegend();
  if (replay.on) renderActivity();
});

// ---------- domain details ----------
const detail = panels.detail;
let detailHost = null;

async function selectHost(host) {
  detailHost = host;
  graph.focus(host) || graph.select(host);
  showTab('detail');
  syncUrl();
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
  shell.refreshStats();
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
    renderTiles(tiles, [
      ['Companies', count(c.companies)],
      ['Open jobs', count(c.open_jobs)],
      ['Job boards', count(c.boards)],
      ['Domains', count(c.domains)],
      ['Pages', count(c.pages)],
    ]);
    feed.reset(events);
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

replayBtn.addEventListener('click', () => (replay.on ? exitReplay() : enterReplay()));
document.getElementById('btn-live').addEventListener('click', exitReplay);
playBtn.addEventListener('click', () => (replay.playing ? stopPlaying() : play()));
scrubber.addEventListener('input', () => {
  stopPlaying();
  setReplayTime(Number(scrubber.value));
});
new ResizeObserver(() => replay.on && renderActivity()).observe(activityEl);

// ---------- live updates ----------
shell.on('event', (msg) => {
  // A replay shows the past: live events wait until "back to live".
  if (replay.on) return;
  const e = msg.event;
  feed.add(msg.ts, e);
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
});
shell.on('resync', () => refreshGraph());

async function start() {
  await refreshGraph();
  await reloadFeed();
  setInterval(refreshGraph, GRAPH_EVERY_MS);
  const params = new URLSearchParams(location.search);
  const host = params.get('host');
  if (host) {
    if (params.get('view') === 'pages') openPages(host);
    else selectHost(host);
  }
}
start();
