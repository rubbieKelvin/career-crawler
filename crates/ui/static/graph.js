// Graph views on sigma.js (WebGL) over graphology, laid out with ForceAtlas2 in short
// bursts on animation frames.
// - DomainGraph: one node per domain, coloured by classification, sized by pages + open
//   jobs; edges are cross-domain links. Used live and for history replay.
// - PageGraph: one domain's pages, its not-yet-fetched internal links, and the external
//   sites it links to (collapsed to one node each).

import { DirectedGraph } from 'https://cdn.jsdelivr.net/npm/graphology@0.26.0/+esm';
import Sigma from 'https://cdn.jsdelivr.net/npm/sigma@3.0.3/+esm';
import forceAtlas2 from 'https://cdn.jsdelivr.net/npm/graphology-layout-forceatlas2@0.10.1/+esm';

/** Domain classification → legend label and colour token. Order is the legend order. */
export const STATUSES = [
  { key: 'company', label: 'Company', token: '--node-company' },
  { key: 'probing', label: 'Not sure yet', token: '--node-probing' },
  { key: 'not_company', label: 'Not a company', token: '--node-not-company' },
  { key: 'discovered', label: 'Linked, not fetched', token: '--node-discovered' },
];

/** Page kinds in the drill-down view. Three categorical slots (careers, job, external
 *  site), neutrals for ordinary and unfetched pages, and the status red for failures. */
export const PAGE_KINDS = [
  { key: 'careers', label: 'Careers page', token: '--node-company' },
  { key: 'job', label: 'Job posting', token: '--node-probing' },
  { key: 'external', label: 'Other site', token: '--node-not-company' },
  { key: 'page', label: 'Page', token: '--ink-2' },
  { key: 'pending', label: 'Linked, not fetched', token: '--node-discovered' },
  { key: 'failed', label: 'Failed', token: '--critical' },
];

const PULSE_MS = 1600;
const LAYOUT_FRAMES_ON_CHANGE = 90;
const LAYOUT_FRAMES_ON_LOAD = 240;

export function token(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export function statusColor(status) {
  const s = STATUSES.find((x) => x.key === status) || STATUSES[3];
  return token(s.token);
}

/** home/other/duplicate pages share the neutral "page" style. */
export function pageKind(kind) {
  return ['careers', 'job', 'external', 'pending', 'failed'].includes(kind) ? kind : 'page';
}

export function pageColor(kind) {
  return token(PAGE_KINDS.find((k) => k.key === pageKind(kind)).token);
}

/** Shared sigma plumbing: tooltip, hover/selection neighbourhood, pulses, layout. */
class SigmaView {
  constructor(container, { onClick, onDoubleClick, describe, labelThreshold = 9 }) {
    this.container = container;
    this.describe = describe;
    this.graph = new DirectedGraph();
    this.pulses = new Map();
    this.hovered = null;
    this.selected = null;
    this.layoutFrames = 0;

    this.sigma = new Sigma(this.graph, container, {
      defaultEdgeType: 'line',
      labelFont: '"IBM Plex Mono", ui-monospace, monospace',
      labelSize: 11,
      labelWeight: '400',
      labelColor: { color: token('--ink-2') },
      labelDensity: 0.6,
      labelRenderedSizeThreshold: labelThreshold,
      zIndex: true,
      minCameraRatio: 0.05,
      maxCameraRatio: 5,
      // The view may start hidden (the other view is showing).
      allowInvalidContainer: true,
      // The HTML tooltip replaces sigma's hover box, which is light-only.
      defaultDrawNodeHover: () => {},
      nodeReducer: (key, data) => this.reduceNode(key, data),
      edgeReducer: (key, data) => this.reduceEdge(key, data),
    });

    this.tip = document.createElement('div');
    this.tip.className = 'node-tip';
    this.tip.hidden = true;
    container.appendChild(this.tip);

    this.sigma.on('enterNode', ({ node, event }) => {
      this.hovered = node;
      this.showTip(node, event);
      this.refresh();
    });
    this.sigma.on('leaveNode', () => {
      this.hovered = null;
      this.tip.hidden = true;
      this.refresh();
    });
    this.sigma.on('clickNode', ({ node }) => onClick && onClick(node, this.graph.getNodeAttributes(node)));
    this.sigma.on('doubleClickNode', ({ node, event }) => {
      if (!onDoubleClick) return;
      event.preventSigmaDefault();
      onDoubleClick(node, this.graph.getNodeAttributes(node));
    });
    this.sigma.on('clickStage', () => this.selectKey(null));
    container.addEventListener('mouseleave', () => { this.tip.hidden = true; });
  }

  get empty() {
    return this.graph.order === 0;
  }

  refresh() {
    this.sigma.refresh({ skipIndexation: true });
  }

  /** Makes the canvas match its container after it was hidden or resized. */
  resize() {
    this.sigma.resize();
    this.sigma.refresh();
  }

  kill() {
    this.layoutFrames = 0;
    this.sigma.kill();
    this.tip.remove();
  }

  /**
   * Merges nodes `[{key, attrs, near}]` and edges `[{key, source, target, attrs}]`. New
   * nodes start next to an already placed neighbour (`near` lists candidate keys).
   * With `prune`, nodes and edges not in the input are removed (scrubbing back in time).
   */
  merge(nodes, edges, { prune = false } = {}) {
    const loadingFirst = this.graph.order === 0;
    let changed = 0;
    if (prune) {
      const keep = new Set(nodes.map((n) => n.key));
      this.graph.forEachNode((key) => { if (!keep.has(key)) { this.graph.dropNode(key); changed++; } });
      const keepEdges = new Set(edges.map((e) => e.key));
      this.graph.forEachEdge((key) => { if (!keepEdges.has(key)) this.graph.dropEdge(key); });
    }
    const spread = 40 + Math.sqrt(this.graph.order + nodes.length) * 12;
    for (const n of nodes) {
      if (this.graph.hasNode(n.key)) {
        this.graph.mergeNodeAttributes(n.key, n.attrs);
        continue;
      }
      const anchor = (n.near || []).find((k) => this.graph.hasNode(k));
      const base = anchor ? this.graph.getNodeAttributes(anchor) : { x: 0, y: 0 };
      const jitter = anchor ? 12 : spread;
      this.graph.addNode(n.key, {
        ...n.attrs,
        x: base.x + (Math.random() - 0.5) * jitter,
        y: base.y + (Math.random() - 0.5) * jitter,
      });
      changed++;
    }
    for (const e of edges) {
      if (!this.graph.hasNode(e.source) || !this.graph.hasNode(e.target)) continue;
      if (this.graph.hasEdge(e.key)) this.graph.mergeEdgeAttributes(e.key, e.attrs);
      else {
        this.graph.addDirectedEdgeWithKey(e.key, e.source, e.target, e.attrs);
        changed++;
      }
    }
    if (this.hovered && !this.graph.hasNode(this.hovered)) this.hovered = null;
    if (this.selected && !this.graph.hasNode(this.selected)) this.selected = null;
    if (changed) this.relayout(loadingFirst ? LAYOUT_FRAMES_ON_LOAD : LAYOUT_FRAMES_ON_CHANGE);
    else this.refresh();
    return changed;
  }

  pulseKey(key) {
    if (!key || !this.graph.hasNode(key)) return;
    this.pulses.set(key, performance.now() + PULSE_MS);
    if (this.pulses.size === 1) requestAnimationFrame(() => this.pulseFrame());
  }

  pulseFrame() {
    const now = performance.now();
    for (const [key, until] of this.pulses) if (until < now) this.pulses.delete(key);
    this.refresh();
    if (this.pulses.size) requestAnimationFrame(() => this.pulseFrame());
  }

  selectKey(key) {
    this.selected = key && this.graph.hasNode(key) ? key : null;
    this.refresh();
  }

  focusKey(key) {
    if (!key || !this.graph.hasNode(key)) return false;
    this.selectKey(key);
    const pos = this.sigma.getNodeDisplayData(key);
    if (pos) this.sigma.getCamera().animate({ x: pos.x, y: pos.y, ratio: 0.35 }, { duration: 500 });
    return true;
  }

  fit() {
    this.sigma.getCamera().animatedReset({ duration: 400 });
  }

  relayout(frames) {
    const running = this.layoutFrames > 0;
    this.layoutFrames = Math.max(this.layoutFrames, frames);
    if (!running) requestAnimationFrame(() => this.layoutFrame());
  }

  layoutFrame() {
    if (this.layoutFrames <= 0 || this.graph.order < 2) {
      this.layoutFrames = 0;
      return;
    }
    const settings = {
      ...forceAtlas2.inferSettings(this.graph),
      barnesHutOptimize: this.graph.order > 300,
      gravity: 1,
      slowDown: 8,
    };
    forceAtlas2.assign(this.graph, { iterations: 2, settings });
    this.layoutFrames--;
    requestAnimationFrame(() => this.layoutFrame());
  }

  /** Hover or selection dims everything outside that node's neighbourhood. */
  reduceNode(key, data) {
    const res = { ...data };
    if (this.pulses.has(key)) {
      res.size = data.size * 1.6;
      res.forceLabel = true;
      res.zIndex = 2;
    }
    const focus = this.hovered || this.selected;
    if (focus) {
      if (key === focus) {
        res.forceLabel = true;
        res.zIndex = 3;
      } else if (!this.graph.areNeighbors(key, focus)) {
        res.color = token('--grid');
        res.label = '';
        res.forceLabel = false;
        res.zIndex = 0;
      } else {
        res.forceLabel = true;
        res.zIndex = 1;
      }
    }
    return res;
  }

  reduceEdge(key, data) {
    const focus = this.hovered || this.selected;
    if (!focus) return data;
    const [s, t] = this.graph.extremities(key);
    if (s !== focus && t !== focus) return { ...data, hidden: true };
    return { ...data, color: token('--muted') };
  }

  showTip(key, event) {
    const [title, ...lines] = this.describe(this.graph.getNodeAttributes(key));
    const head = document.createElement('strong');
    head.textContent = title;
    this.tip.replaceChildren(head, ...lines.filter(Boolean).map((text) => {
      const line = document.createElement('div');
      line.className = 'sub';
      line.textContent = text;
      return line;
    }));
    this.tip.hidden = false;
    const box = this.container.getBoundingClientRect();
    const left = Math.min(event.x + 14, box.width - this.tip.offsetWidth - 8);
    const top = Math.max(8, event.y - this.tip.offsetHeight - 10);
    this.tip.style.left = `${Math.max(0, left)}px`;
    this.tip.style.top = `${top}px`;
  }
}

function domainSize(n) {
  if (n.status === 'discovered') return 2.5;
  return 4 + Math.min(16, 2.2 * Math.log2(1 + n.pages) + 3 * Math.log10(1 + n.jobs));
}

export class DomainGraph extends SigmaView {
  constructor(container, { onSelect, onOpen }) {
    super(container, {
      onClick: (_, attrs) => onSelect(attrs.host),
      onDoubleClick: (_, attrs) => onOpen && onOpen(attrs.host),
      describe: (a) => {
        const status = STATUSES.find((s) => s.key === a.status) || STATUSES[3];
        return [
          a.label,
          a.host === a.label ? status.label : `${a.host} · ${status.label}`,
          `${a.pages} pages · ${a.jobs} open jobs`,
          a.status === 'discovered' ? null : 'double-click to explore its pages',
        ];
      },
    });
    this.keyByHost = new Map();
  }

  /** Merges a `/api/graph` snapshot. `prune` drops domains absent from it (replay). */
  update(snapshot, { prune = false } = {}) {
    const neighbours = new Map();
    for (const e of snapshot.edges) {
      const s = String(e.source);
      const t = String(e.target);
      if (!neighbours.has(s)) neighbours.set(s, []);
      if (!neighbours.has(t)) neighbours.set(t, []);
      neighbours.get(s).push(t);
      neighbours.get(t).push(s);
    }
    if (prune) this.keyByHost.clear();
    const nodes = snapshot.nodes.map((n) => {
      const key = String(n.id);
      this.keyByHost.set(n.host, key);
      return {
        key,
        near: neighbours.get(key),
        attrs: {
          label: n.name || n.host,
          host: n.host,
          status: n.status,
          pages: n.pages,
          jobs: n.jobs,
          size: domainSize(n),
          color: statusColor(n.status),
        },
      };
    });
    const edgeColor = token('--edge');
    const edges = snapshot.edges.map((e) => ({
      key: `${e.source}>${e.target}`,
      source: String(e.source),
      target: String(e.target),
      attrs: { size: 0.6 + Math.log2(Math.max(1, e.weight)) * 0.4, color: edgeColor },
    }));
    return this.merge(nodes, edges, { prune });
  }

  has(host) {
    return this.keyByHost.has(host);
  }

  /** Live touches from events, before the next snapshot confirms them. */
  setStatus(host, status) {
    const key = this.keyByHost.get(host);
    if (!key) return;
    this.graph.mergeNodeAttributes(key, { status, color: statusColor(status) });
    this.pulseKey(key);
  }

  pulse(host) {
    this.pulseKey(this.keyByHost.get(host));
  }

  select(host) {
    this.selectKey(host ? this.keyByHost.get(host) : null);
  }

  focus(host) {
    return this.focusKey(this.keyByHost.get(host));
  }

  /** Re-reads colour tokens, e.g. after a theme switch. */
  applyTheme() {
    const edgeColor = token('--edge');
    this.graph.forEachNode((key, a) => this.graph.setNodeAttribute(key, 'color', statusColor(a.status)));
    this.graph.forEachEdge((key) => this.graph.setEdgeAttribute(key, 'color', edgeColor));
    this.sigma.setSetting('labelColor', { color: token('--ink-2') });
  }
}

function pageSize(n) {
  if (n.kind === 'home') return 11;
  if (n.kind === 'external') return 3 + Math.min(9, 2 * Math.log2(1 + n.links));
  return { careers: 8, job: 5, pending: 2.5 }[n.kind] || 4;
}

export class PageGraph extends SigmaView {
  constructor(container, { onOpenSite }) {
    super(container, {
      labelThreshold: 7,
      onClick: (key) => this.selectKey(key),
      onDoubleClick: (_, a) => {
        if (a.kind === 'external') onOpenSite(a.url);
        else window.open(a.url, '_blank', 'noopener,noreferrer');
      },
      describe: (a) => {
        const kind = a.kind === 'home' ? 'Home page' : PAGE_KINDS.find((k) => k.key === pageKind(a.kind)).label;
        if (a.kind === 'external') return [a.label, `${kind} · ${a.links} links`, 'double-click to explore it'];
        const state = a.kind === 'pending' ? `in the queue: ${a.state}` : a.error ? `${a.error}` : a.httpStatus ? `HTTP ${a.httpStatus}` : null;
        return [a.label, [kind, state].filter(Boolean).join(' · '), a.url, 'double-click to open'];
      },
    });
  }

  load(data) {
    const nodes = data.nodes.map((n) => ({
      key: n.id,
      attrs: {
        label: n.label,
        kind: n.kind,
        url: n.url,
        state: n.state,
        error: n.error,
        httpStatus: n.http_status,
        links: n.links,
        size: pageSize(n),
        color: pageColor(n.kind),
        forceLabel: n.kind === 'home' || n.kind === 'careers',
      },
    }));
    const edgeColor = token('--edge');
    const edges = data.edges.map((e) => ({
      key: `${e.source}>${e.target}`,
      source: e.source,
      target: e.target,
      attrs: { size: 0.5 + Math.log2(Math.max(1, e.weight)) * 0.4, color: edgeColor },
    }));
    const home = data.nodes.find((n) => n.kind === 'home');
    for (const n of nodes) n.near = home ? [home.id] : [];
    return this.merge(nodes, edges, { prune: true });
  }

  applyTheme() {
    const edgeColor = token('--edge');
    this.graph.forEachNode((key, a) => this.graph.setNodeAttribute(key, 'color', pageColor(a.kind)));
    this.graph.forEachEdge((key) => this.graph.setEdgeAttribute(key, 'color', edgeColor));
    this.sigma.setSetting('labelColor', { color: token('--ink-2') });
  }
}
