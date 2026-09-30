// Helpers shared by every page: formatting, DOM building, API calls. All untrusted text
// (hosts, titles, URLs) goes into the DOM via textContent.

export const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
export const whole = new Intl.NumberFormat();
export const clock = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
export const dateTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

/** Decimal units (1 kB = 1000 B), so round axis ticks read as round numbers. */
export function bytes(n) {
  if (n == null) return '–';
  const units = ['B', 'kB', 'MB', 'GB', 'TB'];
  let i = 0;
  while (n >= 1000 && i < units.length - 1) { n /= 1000; i++; }
  return `${n >= 100 || i === 0 ? Math.round(n) : Number(n.toFixed(1))} ${units[i]}`;
}
export const count = (n) => (n == null ? '–' : n < 10000 ? whole.format(n) : compact.format(n));

export function salary(job) {
  if (job.salary_min == null && job.salary_max == null) return null;
  const f = (v) => compact.format(v);
  const range = job.salary_min != null && job.salary_max != null && job.salary_min !== job.salary_max
    ? `${f(job.salary_min)}–${f(job.salary_max)}`
    : f(job.salary_min ?? job.salary_max);
  return `${job.salary_currency || ''} ${range}${job.salary_period ? ` / ${job.salary_period}` : ''}`.trim();
}

export function h(tag, props = {}, ...children) {
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

export async function api(path, options) {
  const resp = await fetch(path, options);
  if (!resp.ok) throw new Error(`${path}: ${resp.status}`);
  return resp.json();
}

export function shortUrl(url) {
  try { const u = new URL(url); return u.host + (u.pathname === '/' ? '' : u.pathname); } catch { return url; }
}

export function link(url, text) {
  return h('a', { href: url, target: '_blank', rel: 'noopener noreferrer', text: text || shortUrl(url) });
}

/** Where a domain's details and page graph live. */
export const domainHref = (host) => `/graph?host=${encodeURIComponent(host)}`;

/** The totals row, used by the dashboard and the graph page. */
export function renderTiles(el, items) {
  el.replaceChildren(...items.map(([label, value]) =>
    h('div', { class: 'tile' }, h('div', { class: 'label', text: label }), h('div', { class: 'value', text: value }))));
}

export function statTiles(stats) {
  const c = stats.counts;
  const s = stats.metrics.latest;
  return [
    ['Companies', count(c.companies)],
    ['Open jobs', count(c.open_jobs)],
    ['Job boards', count(c.boards)],
    ['Domains', count(c.domains)],
    ['Pages', count(c.pages)],
    ['Queued', count(c.frontier_queued)],
    ['Data this run', stats.crawler.run_id && s && s.run_id === stats.crawler.run_id ? bytes(s.bytes_rx_wire) : '–'],
  ];
}
