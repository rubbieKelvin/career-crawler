// The resource charts (download rate, pages per minute, LLM usage, CPU, memory) and the
// samples table. `mountCharts` fills `el`, loads the last 30 minutes and then follows the
// live `metrics` messages; `only` picks a subset of series by title.

import { LineChart } from '/static/charts.js';
import { api, bytes, clock, h, whole } from '/static/common.js';

export const HISTORY_MS = 30 * 60 * 1000;
/** Samples further apart than this (or from different runs) are not joined by a line. */
const SAMPLE_GAP_MS = 10000;

/** Derived per-sample series; rates are null across a run boundary. */
export const SERIES = [
  { title: 'Download rate', format: (v) => `${bytes(v)}/s`,
    value: (p, s) => (p && p.run_id === s.run_id && s.ts > p.ts ? Math.max(0, (s.bytes_rx_wire - p.bytes_rx_wire) / ((s.ts - p.ts) / 1000)) : null) },
  { title: 'Pages per minute', format: (v) => whole.format(Math.round(v)),
    value: (p, s) => (p && p.run_id === s.run_id && s.ts > p.ts ? Math.max(0, (s.pages - p.pages) / ((s.ts - p.ts) / 60000)) : null) },
  // LLM counters are cumulative per run (all zero when the LLM is off).
  { title: 'LLM tokens per minute', format: (v) => whole.format(Math.round(v)),
    value: (p, s) => (p && p.run_id === s.run_id && s.ts > p.ts
      ? Math.max(0, (s.llm_tokens_in + s.llm_tokens_out - p.llm_tokens_in - p.llm_tokens_out) / ((s.ts - p.ts) / 60000)) : null) },
  { title: 'LLM cache hit rate', format: (v) => `${Math.round(v)}%`,
    value: (_, s) => { const asked = s.llm_calls + s.llm_cache_hits; return asked > 0 ? (100 * s.llm_cache_hits) / asked : null; } },
  { title: 'CPU', format: (v) => `${Math.round(v)}%`, value: (_, s) => s.cpu_pct },
  { title: 'Memory (RSS)', format: bytes, value: (_, s) => s.rss_bytes },
];

export async function mountCharts({ el, shell, only = null, table = null }) {
  const specs = only ? SERIES.filter((s) => only.includes(s.title)) : SERIES;
  const charts = specs.map((spec) => ({ spec, chart: new LineChart(el, spec) }));
  let samples = [];

  function render() {
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
    if (table && !table.wrap.hidden) renderTable();
  }

  function renderTable() {
    const recent = samples.slice(-30).reverse();
    const head = h('tr', {}, h('th', { text: 'Time' }), ...specs.map((s) => h('th', { text: s.title })));
    const rows = recent.map((s) => {
      const i = samples.indexOf(s);
      return h('tr', {}, h('td', { text: clock.format(s.ts) }),
        ...specs.map((spec) => { const v = spec.value(samples[i - 1], s); return h('td', { text: v == null ? '–' : spec.format(v) }); }));
    });
    table.wrap.replaceChildren(h('table', { class: 'samples' }, h('thead', {}, head), h('tbody', {}, ...rows)));
  }

  if (table) {
    table.button.addEventListener('click', () => {
      table.wrap.hidden = !table.wrap.hidden;
      table.button.setAttribute('aria-expanded', String(!table.wrap.hidden));
      table.button.textContent = table.wrap.hidden ? 'Show table' : 'Hide table';
      if (!table.wrap.hidden) renderTable();
    });
  }

  shell.on('metrics', (sample) => { samples.push(sample); render(); });
  shell.on('theme', () => charts.forEach((c) => c.chart.render()));
  try {
    samples = await api(`/api/metrics/history?from=${Date.now() - HISTORY_MS}`);
    render();
  } catch (e) { console.warn(e); }
}
