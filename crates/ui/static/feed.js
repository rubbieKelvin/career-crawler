// The live event feed: a filterable list of what the crawler is doing, newest first.

import { STATUSES } from '/static/graph.js';
import { clock, h, shortUrl, whole } from '/static/common.js';

const FEED_MAX = 300;
const CATEGORY = {
  jobs_found: 'jobs', jobs_enriched: 'jobs', careers_found: 'careers', domain_classified: 'classify', fetch_failed: 'errors',
  crawler_started: 'crawler', crawler_stopped: 'crawler', seeds_loaded: 'crawler', control_applied: 'crawler',
  profile_changed: 'crawler',
};
const statusLabel = (key) => (STATUSES.find((s) => s.key === key) || { label: key }).label;

export const FEED_FILTERS = [
  ['all', 'All events'], ['jobs', 'Jobs'], ['careers', 'Careers pages'], ['classify', 'Classifications'],
  ['errors', 'Failures'], ['crawler', 'Crawler'],
];

/** `onHost(host)` is called when a domain name in the feed is clicked. */
export function createFeed({ list, filter, onHost }) {
  for (const [value, label] of FEED_FILTERS) filter.append(h('option', { value, text: label }));

  const hostButton = (host) =>
    (host ? h('button', { class: 'host', type: 'button', text: host, onclick: () => onHost(host) }) : null);

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
      case 'profile_changed':
        return ['Profile', `${e.name} · ${whole.format(e.jobs_scored)} jobs ranked, ${whole.format(e.frontier_rescored)} queued links re-scored`];
      case 'crawler_started': return ['Crawler', 'started'];
      case 'crawler_stopped': return ['Crawler', `stopped: ${e.reason.replaceAll('_', ' ')}`];
      case 'seeds_loaded': return ['Seeds', `${e.parsed} loaded, ${e.enqueued} new`];
      case 'control_applied': return ['Control', `${e.command} (${e.source})`];
      default: return [e.kind, JSON.stringify(e)];
    }
  }

  const applyFilter = (item) => {
    item.hidden = filter.value !== 'all' && item.dataset.category !== filter.value;
  };
  filter.addEventListener('change', () => [...list.children].forEach(applyFilter));

  return {
    add(ts, event) {
      const [what, ...content] = describe(event);
      const item = h('li', { 'data-category': CATEGORY[event.kind] || 'crawl' },
        h('span', { class: 'time', text: clock.format(ts) }),
        h('div', {}, h('div', { class: 'what', text: what }), h('div', { class: 'text' }, ...content)));
      applyFilter(item);
      list.prepend(item);
      while (list.children.length > FEED_MAX) list.lastChild.remove();
    },
    /** Replaces the list with `messages` (oldest first from the API, like a live stream). */
    reset(messages) {
      list.replaceChildren();
      messages.forEach((m) => this.add(m.ts, m.event));
    },
  };
}
