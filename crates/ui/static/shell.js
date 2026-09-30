// The frame every page shares: navigation, crawler state and controls, theme toggle, the
// stats poll and the live WebSocket. Pages subscribe with `shell.on(...)`:
//   'stats' (stats)   'event' (msg)   'metrics' (sample)   'resync' (after a reconnect or lag)

import { api, h } from '/static/common.js';

const STATS_EVERY_MS = 5000;

const PAGES = [
  { key: 'dashboard', href: '/', label: 'Dashboard' },
  { key: 'graph', href: '/graph', label: 'Graph' },
  { key: 'search', href: '/search', label: 'Job search' },
  { key: 'profile', href: '/profile', label: 'Profile' },
  { key: 'resources', href: '/resources', label: 'Resources' },
];

const SPIDER = '<svg class="spider" viewBox="-14 -14 28 28" aria-hidden="true"><g class="walker"><g transform="translate(-2.5 -3)"><path class="leg a" d="M0 0Q-4.2 -8 -7 -5"/></g><g transform="translate(2.5 -3)"><path class="leg r b" d="M0 0Q4.2 -8 7 -5"/></g><g transform="translate(-2.5 -1)"><path class="leg b" d="M0 0Q-5.4 -4 -9 -1"/></g><g transform="translate(2.5 -1)"><path class="leg r a" d="M0 0Q5.4 -4 9 -1"/></g><g transform="translate(-2.5 1)"><path class="leg a" d="M0 0Q-5.4 0 -9 3"/></g><g transform="translate(2.5 1)"><path class="leg r b" d="M0 0Q5.4 0 9 3"/></g><g transform="translate(-2.5 3)"><path class="leg b" d="M0 0Q-4.2 3 -7 6"/></g><g transform="translate(2.5 3)"><path class="leg r a" d="M0 0Q4.2 3 7 6"/></g><ellipse class="abdomen" cx="0" cy="3" rx="3.6" ry="5"/><circle class="head" cx="0" cy="-3.5" r="2.4"/></g></svg>';

const darkQuery = matchMedia('(prefers-color-scheme: dark)');
const isDark = () => {
  const t = document.documentElement.dataset.theme;
  return t ? t === 'dark' : darkQuery.matches;
};

/** Builds the header and footer into `#shell-top` / `#shell-bottom` and starts the live link. */
export function initShell(page) {
  const listeners = { stats: [], event: [], metrics: [], resync: [], theme: [] };
  const shell = {
    crawler: { running: false, paused: false },
    on(type, fn) { listeners[type].push(fn); return shell; },
    emit(type, arg) { listeners[type].forEach((fn) => fn(arg)); },
    refreshStats,
  };

  const nav = h('nav', { class: 'nav', 'aria-label': 'Pages' },
    ...PAGES.map((p) => h('a', { href: p.href, text: p.label, ...(p.key === page ? { 'aria-current': 'page' } : {}) })));
  const pill = h('span', { class: 'pill', id: 'crawler-state', 'data-state': 'stopped', role: 'status', 'aria-live': 'polite' });
  pill.innerHTML = `${SPIDER}<span class="text visually-hidden">Connecting…</span>`;
  const pauseBtn = h('button', { class: 'btn', type: 'button', text: 'Pause', disabled: '' });
  const stopBtn = h('button', { class: 'btn', type: 'button', text: 'Stop', disabled: '' });
  document.getElementById('shell-top').replaceChildren(
    h('header', { class: 'topbar' },
      h('a', { class: 'brand', href: '/', 'aria-label': 'Career Crawler, dashboard' }, h('h1', { text: 'Career Crawler' })),
      nav,
      h('div', { class: 'actions' }, pill, pauseBtn, stopBtn)));

  const themeBtn = h('button', { class: 'btn', type: 'button' });
  document.getElementById('shell-bottom').replaceChildren(
    h('footer', { class: 'foot' }, h('span', { text: 'career crawler' }), themeBtn));

  function renderCrawler(status) {
    shell.crawler = status;
    const state = status.paused ? 'paused' : status.running ? 'running' : 'stopped';
    pill.dataset.state = state;
    const label = state === 'paused' ? 'Paused' : state === 'running' ? 'Crawling' : 'Idle';
    pill.querySelector('.text').textContent = label;
    pill.title = label;
    pauseBtn.disabled = !status.running;
    pauseBtn.textContent = status.paused ? 'Resume' : 'Pause';
    stopBtn.disabled = !status.running;
  }

  async function refreshStats() {
    try {
      const stats = await api('/api/stats');
      renderCrawler(stats.crawler);
      shell.emit('stats', stats);
    } catch (e) { console.warn(e); }
  }

  pauseBtn.addEventListener('click', async () => {
    await api(`/api/control/${shell.crawler.paused ? 'resume' : 'pause'}`, { method: 'POST' });
    pauseBtn.disabled = true;
    setTimeout(refreshStats, 800);
  });
  stopBtn.addEventListener('click', async () => {
    if (!confirm('Stop the crawler? In-flight pages finish first.')) return;
    await api('/api/control/stop', { method: 'POST' });
    stopBtn.disabled = true;
    setTimeout(refreshStats, 800);
  });

  // Theme
  const syncThemeBtn = () => { themeBtn.textContent = isDark() ? 'light' : 'dark'; };
  syncThemeBtn();
  themeBtn.addEventListener('click', () => {
    const next = isDark() ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    try { localStorage.setItem('theme', next); } catch (e) { /* storage blocked: choice lasts this page load */ }
    syncThemeBtn();
    shell.emit('theme');
  });
  darkQuery.addEventListener('change', () => { syncThemeBtn(); shell.emit('theme'); });

  // Live link
  let statsTimer = null;
  shell.throttledStats = () => {
    if (statsTimer) return;
    statsTimer = setTimeout(() => { statsTimer = null; refreshStats(); }, 1000);
  };
  function connect(delay = 1000) {
    const ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`);
    ws.onopen = () => { delay = 1000; refreshStats(); shell.emit('resync'); };
    ws.onmessage = (raw) => {
      const msg = JSON.parse(raw.data);
      if (msg.type === 'event') {
        if (['crawler_started', 'crawler_stopped', 'control_applied'].includes(msg.event.kind)) refreshStats();
        shell.emit('event', msg);
      } else if (msg.type === 'metrics') {
        shell.emit('metrics', msg.sample);
        shell.throttledStats();
      } else if (msg.type === 'lagged') {
        refreshStats();
        shell.emit('resync');
      }
    };
    ws.onclose = () => setTimeout(() => connect(Math.min(delay * 2, 15000)), delay);
  }
  connect();
  refreshStats();
  setInterval(refreshStats, STATS_EVERY_MS);
  return shell;
}
