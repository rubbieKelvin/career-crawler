// The domain graph: sigma.js (WebGL) over a graphology graph, laid out with
// ForceAtlas2 in short bursts on animation frames. Nodes are domains coloured by
// classification and sized by pages + open jobs; edges are cross-domain links.

import { DirectedGraph } from 'https://cdn.jsdelivr.net/npm/graphology@0.26.0/+esm';
import Sigma from 'https://cdn.jsdelivr.net/npm/sigma@3.0.3/+esm';
import forceAtlas2 from 'https://cdn.jsdelivr.net/npm/graphology-layout-forceatlas2@0.10.1/+esm';

/** Classification → legend label and colour token. Order is the legend order. */
export const STATUSES = [
  { key: 'company', label: 'Company', token: '--node-company' },
  { key: 'probing', label: 'Not sure yet', token: '--node-probing' },
  { key: 'not_company', label: 'Not a company', token: '--node-not-company' },
  { key: 'discovered', label: 'Linked, not fetched', token: '--node-discovered' },
];

const PULSE_MS = 1600;
const LAYOUT_FRAMES_ON_CHANGE = 90;
const LAYOUT_FRAMES_ON_LOAD = 240;

function token(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

export function statusColor(status) {
  const s = STATUSES.find((x) => x.key === status) || STATUSES[3];
  return token(s.token);
}

function nodeSize(n) {
  if (n.status === 'discovered') return 2.5;
  return 4 + Math.min(16, 2.2 * Math.log2(1 + n.pages) + 3 * Math.log10(1 + n.jobs));
}

export class DomainGraph {
  constructor(container, { onSelect }) {
    this.container = container;
    this.onSelect = onSelect;
    this.graph = new DirectedGraph();
    this.keyByHost = new Map();
    this.pulses = new Map();
    this.hovered = null;
    this.selected = null;
    this.layoutFrames = 0;

    this.sigma = new Sigma(this.graph, container, {
      defaultEdgeType: 'line',
      labelFont: 'system-ui, -apple-system, "Segoe UI", sans-serif',
      labelSize: 12,
      labelWeight: '500',
      labelColor: { color: token('--ink-2') },
      labelDensity: 0.6,
      labelRenderedSizeThreshold: 9,
      zIndex: true,
      minCameraRatio: 0.05,
      maxCameraRatio: 5,
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
      this.sigma.refresh({ skipIndexation: true });
    });
    this.sigma.on('leaveNode', () => {
      this.hovered = null;
      this.tip.hidden = true;
      this.sigma.refresh({ skipIndexation: true });
    });
    this.sigma.on('clickNode', ({ node }) => this.onSelect(this.graph.getNodeAttribute(node, 'host')));
    this.sigma.on('clickStage', () => this.select(null));
    container.addEventListener('mouseleave', () => { this.tip.hidden = true; });
  }

  get empty() {
    return this.graph.order === 0;
  }

  /** Merges a `/api/graph` snapshot: new nodes are placed near a neighbour, existing ones updated. */
  update(snapshot) {
    const loadingFirst = this.graph.order === 0;
    let added = 0;
    const positioned = (id) => this.graph.hasNode(String(id)) && this.graph.getNodeAttribute(String(id), 'x') != null;
    const spread = 40 + Math.sqrt(this.graph.order + snapshot.nodes.length) * 12;
    for (const n of snapshot.nodes) {
      const key = String(n.id);
      this.keyByHost.set(n.host, key);
      const attrs = {
        label: n.name || n.host,
        host: n.host,
        status: n.status,
        pages: n.pages,
        jobs: n.jobs,
        size: nodeSize(n),
        color: statusColor(n.status),
      };
      if (this.graph.hasNode(key)) {
        this.graph.mergeNodeAttributes(key, attrs);
        continue;
      }
      const edge = snapshot.edges.find((e) => (e.source === n.id && positioned(e.target)) || (e.target === n.id && positioned(e.source)));
      const anchor = edge ? String(edge.source === n.id ? edge.target : edge.source) : null;
      const base = anchor ? this.graph.getNodeAttributes(anchor) : { x: 0, y: 0 };
      const jitter = anchor ? 12 : spread;
      this.graph.addNode(key, { ...attrs, x: base.x + (Math.random() - 0.5) * jitter, y: base.y + (Math.random() - 0.5) * jitter });
      added++;
    }
    const edgeColor = token('--edge');
    for (const e of snapshot.edges) {
      const s = String(e.source);
      const t = String(e.target);
      if (!this.graph.hasNode(s) || !this.graph.hasNode(t)) continue;
      const key = `${s}>${t}`;
      const attrs = { size: 0.6 + Math.log2(Math.max(1, e.weight)) * 0.4, color: edgeColor };
      if (this.graph.hasEdge(key)) this.graph.mergeEdgeAttributes(key, attrs);
      else {
        this.graph.addDirectedEdgeWithKey(key, s, t, attrs);
        added++;
      }
    }
    if (added) this.relayout(loadingFirst ? LAYOUT_FRAMES_ON_LOAD : LAYOUT_FRAMES_ON_CHANGE);
    return added;
  }

  has(host) {
    return this.keyByHost.has(host);
  }

  hosts() {
    return [...this.keyByHost.keys()];
  }

  /** Live touches from events, before the next snapshot confirms them. */
  setStatus(host, status) {
    const key = this.keyByHost.get(host);
    if (!key) return;
    this.graph.mergeNodeAttributes(key, { status, color: statusColor(status) });
    this.pulse(host);
  }

  pulse(host) {
    const key = this.keyByHost.get(host);
    if (!key) return;
    this.pulses.set(key, performance.now() + PULSE_MS);
    if (this.pulses.size === 1) requestAnimationFrame(() => this.pulseFrame());
  }

  pulseFrame() {
    const now = performance.now();
    for (const [key, until] of this.pulses) if (until < now) this.pulses.delete(key);
    this.sigma.refresh({ skipIndexation: true });
    if (this.pulses.size) requestAnimationFrame(() => this.pulseFrame());
  }

  select(host) {
    this.selected = host ? this.keyByHost.get(host) || null : null;
    this.sigma.refresh({ skipIndexation: true });
  }

  focus(host) {
    const key = this.keyByHost.get(host);
    if (!key) return false;
    this.select(host);
    const pos = this.sigma.getNodeDisplayData(key);
    if (pos) this.sigma.getCamera().animate({ x: pos.x, y: pos.y, ratio: 0.35 }, { duration: 500 });
    return true;
  }

  fit() {
    this.sigma.getCamera().animatedReset({ duration: 400 });
  }

  /** Re-reads colour tokens, e.g. after the OS switches light/dark. */
  applyTheme() {
    const edgeColor = token('--edge');
    this.graph.forEachNode((key, attrs) => this.graph.setNodeAttribute(key, 'color', statusColor(attrs.status)));
    this.graph.forEachEdge((key) => this.graph.setEdgeAttribute(key, 'color', edgeColor));
    this.sigma.setSetting('labelColor', { color: token('--ink-2') });
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
  focusKey() {
    return this.hovered || this.selected;
  }

  reduceNode(key, data) {
    const res = { ...data };
    const until = this.pulses.get(key);
    if (until) {
      res.size = data.size * 1.6;
      res.forceLabel = true;
      res.zIndex = 2;
    }
    const focus = this.focusKey();
    if (focus) {
      if (key === focus) {
        res.forceLabel = true;
        res.zIndex = 3;
      } else if (!this.graph.areNeighbors(key, focus)) {
        res.color = token('--grid');
        res.label = '';
        res.zIndex = 0;
      } else {
        res.forceLabel = true;
        res.zIndex = 1;
      }
    }
    return res;
  }

  reduceEdge(key, data) {
    const focus = this.focusKey();
    if (!focus) return data;
    const [s, t] = this.graph.extremities(key);
    if (s !== focus && t !== focus) return { ...data, hidden: true };
    return { ...data, color: token('--muted') };
  }

  showTip(key, event) {
    const a = this.graph.getNodeAttributes(key);
    const status = STATUSES.find((s) => s.key === a.status) || STATUSES[3];
    const title = document.createElement('strong');
    title.textContent = a.label;
    const host = document.createElement('div');
    host.className = 'sub';
    host.textContent = a.host === a.label ? status.label : `${a.host} · ${status.label}`;
    const counts = document.createElement('div');
    counts.className = 'sub';
    counts.textContent = `${a.pages} pages · ${a.jobs} open jobs`;
    this.tip.replaceChildren(title, host, counts);
    this.tip.hidden = false;
    const box = this.container.getBoundingClientRect();
    const left = Math.min(event.x + 14, box.width - this.tip.offsetWidth - 8);
    const top = Math.max(8, event.y - this.tip.offsetHeight - 10);
    this.tip.style.left = `${left}px`;
    this.tip.style.top = `${top}px`;
  }
}
