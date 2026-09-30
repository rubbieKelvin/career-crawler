// Dashboard: the totals, what the crawler is doing right now, and two headline charts.

import { api, domainHref, renderTiles, statTiles } from '/static/common.js';
import { createFeed } from '/static/feed.js';
import { mountCharts } from '/static/metrics.js';
import { initShell } from '/static/shell.js';

const shell = initShell('dashboard');
const tiles = document.getElementById('tiles');
const feed = createFeed({
  list: document.getElementById('feed'),
  filter: document.getElementById('feed-filter'),
  onHost: (host) => { location.href = domainHref(host); },
});

shell.on('stats', (stats) => renderTiles(tiles, statTiles(stats)));
shell.on('event', (msg) => feed.add(msg.ts, msg.event));
shell.on('resync', reloadFeed);

async function reloadFeed() {
  try { feed.reset(await api('/api/events?limit=150')); } catch (e) { console.warn(e); }
}

mountCharts({ el: document.getElementById('charts'), shell, only: ['Pages per minute', 'Download rate'] });
