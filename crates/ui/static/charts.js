// Small single-series line charts (SVG): 2px line, 10% area wash, hairline grid, clean
// y ticks, an end dot with a surface ring, and a crosshair tooltip that snaps to the
// nearest sample (pointer, or ← → when focused). A `null` value breaks the line
// (a gap between crawler runs).

const SVG = 'http://www.w3.org/2000/svg';
const M = { top: 8, right: 12, bottom: 22, left: 52 };

function el(name, attrs = {}, parent) {
  const node = document.createElementNS(SVG, name);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  if (parent) parent.appendChild(node);
  return node;
}

/** Round-number ticks covering [0, max]. */
function niceTicks(max, count = 3) {
  if (!(max > 0)) return [0, 1];
  const raw = max / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw);
  const ticks = [];
  for (let v = 0; v <= max + step * 0.001; v += step) ticks.push(v);
  if (ticks[ticks.length - 1] < max) ticks.push(ticks[ticks.length - 1] + step);
  return ticks;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const timeFmtLong = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });

export class LineChart {
  /**
   * @param {HTMLElement} host
   * @param {{title: string, format: (v:number)=>string, axisFormat?: (v:number)=>string}} opts
   */
  constructor(host, opts) {
    this.opts = opts;
    this.points = [];
    this.focus = null;
    this.root = document.createElement('div');
    this.root.className = 'chart';
    const h = document.createElement('h3');
    h.textContent = opts.title;
    this.now = document.createElement('div');
    this.now.className = 'now';
    this.svg = el('svg', { role: 'img', tabindex: '0', 'aria-label': opts.title });
    this.tip = document.createElement('div');
    this.tip.className = 'tip';
    this.tip.hidden = true;
    this.root.append(h, this.now, this.svg, this.tip);
    host.appendChild(this.root);

    this.svg.addEventListener('pointermove', (e) => this.hoverAt(e.clientX));
    this.svg.addEventListener('pointerleave', () => this.setFocus(null));
    this.svg.addEventListener('blur', () => this.setFocus(null));
    this.svg.addEventListener('keydown', (e) => {
      const valid = this.validIndexes();
      if (!valid.length || !['ArrowLeft', 'ArrowRight'].includes(e.key)) return;
      e.preventDefault();
      const pos = this.focus == null ? valid.length : valid.indexOf(this.focus);
      const next = e.key === 'ArrowLeft' ? Math.max(0, pos - 1) : Math.min(valid.length - 1, pos + 1);
      this.setFocus(valid[next]);
    });
    new ResizeObserver(() => this.render()).observe(this.svg);
  }

  /** @param {{t:number, v:number|null}[]} points */
  setData(points) {
    this.points = points;
    this.render();
  }

  validIndexes() {
    return this.points.flatMap((p, i) => (p.v == null ? [] : [i]));
  }

  render() {
    const width = this.svg.clientWidth || 300;
    const height = this.svg.clientHeight || 150;
    this.svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
    this.svg.replaceChildren();
    const pts = this.points;
    const values = pts.filter((p) => p.v != null);
    const last = values[values.length - 1];
    this.now.textContent = last ? `Now ${this.opts.format(last.v)}` : 'No samples yet';
    this.svg.setAttribute('aria-label', `${this.opts.title}${last ? `, now ${this.opts.format(last.v)}` : ''}`);

    const plotW = width - M.left - M.right;
    const plotH = height - M.top - M.bottom;
    const ticks = niceTicks(Math.max(0, ...values.map((p) => p.v)));
    const yMax = ticks[ticks.length - 1];
    const t0 = pts.length ? pts[0].t : Date.now() - 1;
    const t1 = pts.length > 1 ? pts[pts.length - 1].t : t0 + 1;
    const x = (t) => M.left + ((t - t0) / (t1 - t0 || 1)) * plotW;
    const y = (v) => M.top + plotH - (v / yMax) * plotH;
    this.scale = { x, y, t0, t1, plotW };

    const axisFmt = this.opts.axisFormat || this.opts.format;
    for (const tick of ticks) {
      el('line', { x1: M.left, x2: width - M.right, y1: y(tick), y2: y(tick), stroke: 'var(--grid)', 'stroke-width': 1 }, this.svg);
      const label = el('text', { x: M.left - 6, y: y(tick) + 4, 'text-anchor': 'end', 'font-size': 11, fill: 'var(--muted)' }, this.svg);
      label.textContent = axisFmt(tick);
    }
    el('line', { x1: M.left, x2: width - M.right, y1: y(0), y2: y(0), stroke: 'var(--axis)', 'stroke-width': 1 }, this.svg);
    if (pts.length > 1) {
      for (const t of [t0, (t0 + t1) / 2, t1]) {
        const label = el('text', { x: x(t), y: height - 6, 'text-anchor': t === t0 ? 'start' : t === t1 ? 'end' : 'middle', 'font-size': 11, fill: 'var(--muted)' }, this.svg);
        label.textContent = timeFmt.format(t);
      }
    }

    // Split into runs of non-null values so gaps stay gaps.
    const runs = [];
    let current = [];
    for (const p of pts) {
      if (p.v == null) {
        if (current.length) runs.push(current);
        current = [];
      } else current.push(p);
    }
    if (current.length) runs.push(current);
    for (const run of runs) {
      const line = run.map((p, i) => `${i ? 'L' : 'M'}${x(p.t).toFixed(1)},${y(p.v).toFixed(1)}`).join('');
      if (run.length > 1) {
        const area = `${line}L${x(run[run.length - 1].t).toFixed(1)},${y(0)}L${x(run[0].t).toFixed(1)},${y(0)}Z`;
        el('path', { d: area, fill: 'var(--series-1)', 'fill-opacity': 0.1 }, this.svg);
      }
      el('path', { d: line, fill: 'none', stroke: 'var(--series-1)', 'stroke-width': 2, 'stroke-linejoin': 'round', 'stroke-linecap': 'round' }, this.svg);
    }
    if (last) el('circle', { cx: x(last.t), cy: y(last.v), r: 4, fill: 'var(--series-1)', stroke: 'var(--surface)', 'stroke-width': 2 }, this.svg);

    this.cross = el('line', { y1: M.top, y2: M.top + plotH, stroke: 'var(--axis)', 'stroke-width': 1, visibility: 'hidden' }, this.svg);
    this.marker = el('circle', { r: 4, fill: 'var(--series-1)', stroke: 'var(--surface)', 'stroke-width': 2, visibility: 'hidden' }, this.svg);
    if (this.focus != null) this.setFocus(this.focus);
  }

  hoverAt(clientX) {
    if (!this.scale) return;
    const box = this.svg.getBoundingClientRect();
    const px = clientX - box.left;
    let best = null;
    let bestDist = Infinity;
    for (const i of this.validIndexes()) {
      const d = Math.abs(this.scale.x(this.points[i].t) - px);
      if (d < bestDist) { bestDist = d; best = i; }
    }
    this.setFocus(best);
  }

  setFocus(index) {
    this.focus = index;
    const p = index == null ? null : this.points[index];
    if (!p || p.v == null || !this.scale) {
      this.tip.hidden = true;
      if (this.cross) this.cross.setAttribute('visibility', 'hidden');
      if (this.marker) this.marker.setAttribute('visibility', 'hidden');
      return;
    }
    const cx = this.scale.x(p.t);
    const cy = this.scale.y(p.v);
    this.cross.setAttribute('x1', cx);
    this.cross.setAttribute('x2', cx);
    this.cross.setAttribute('visibility', 'visible');
    this.marker.setAttribute('cx', cx);
    this.marker.setAttribute('cy', cy);
    this.marker.setAttribute('visibility', 'visible');
    const value = document.createElement('strong');
    value.textContent = this.opts.format(p.v);
    const when = document.createElement('span');
    when.className = 'sub';
    when.textContent = timeFmtLong.format(p.t);
    this.tip.replaceChildren(value, when);
    this.tip.hidden = false;
    const svgBox = this.svg.getBoundingClientRect();
    const rootBox = this.root.getBoundingClientRect();
    const left = svgBox.left - rootBox.left + cx;
    const tipW = this.tip.offsetWidth;
    this.tip.style.left = `${Math.min(Math.max(0, left - tipW / 2), rootBox.width - tipW)}px`;
    this.tip.style.top = `${svgBox.top - rootBox.top + cy - this.tip.offsetHeight - 10}px`;
  }
}
