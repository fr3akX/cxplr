'use strict';

const $ = (sel, el = document) => el.querySelector(sel);
const NODE_H = 54;
const IDEAL_EDGE = 230; // preferred centre distance between connected boxes
const MARGIN_X = 60; // minimum free space between boxes after overlap removal
const MARGIN_Y = 46;
// Repulsion only acts within this many ideal edge lengths; all-pairs repulsion
// otherwise flings leaf nodes of large maps far from their hub.
const REPULSION_RANGE = 1.8;

const state = {
  repo: null,
  scope: '',
  map: null,
  layout: null,
  selected: null, // { type: 'node', id } | { type: 'edge', index }
  flow: null, // index into map.flows
  view: { x: 0, y: 0, k: 1 },
  poll: null,
  names: new Map(), // scope -> display name, for breadcrumbs before a map loads
};

// ---------- utilities ----------

function esc(text) {
  return String(text ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

/** Minimal inline formatting: `code`, **bold**, paragraphs. Input is escaped first. */
function md(text) {
  return String(text ?? '')
    .split(/\n{2,}/)
    .map((para) => '<p>' + esc(para)
      .replace(/`([^`]+)`/g, '<code>$1</code>')
      .replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
      .replace(/\n/g, '<br>') + '</p>')
    .join('');
}

function trunc(text, max) {
  text = String(text ?? '');
  return text.length > max ? text.slice(0, max - 1) + '…' : text;
}

function kindColor(kind) {
  return `var(--k-${kind}, var(--accent))`;
}

async function api(path, options) {
  const res = await fetch(path, options);
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.error || `${res.status} ${res.statusText}`);
  return body;
}

function post(path, body) {
  return api(path, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body ?? {}) });
}

function fmtDuration(ms) {
  const s = Math.round(ms / 1000);
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
}

function fmtTokens(n) {
  return n >= 1e6 ? `${(n / 1e6).toFixed(1)}M` : n >= 1e3 ? `${Math.round(n / 1e3)}k` : String(n);
}

const nodeById = (id) => state.map?.nodes.find((n) => n.id === id);
const childScope = (id) => (state.scope ? `${state.scope}/${id}` : id);

// ---------- navigation ----------

function scopeFromHash() {
  const m = location.hash.match(/scope=([^&]*)/);
  return m ? decodeURIComponent(m[1]) : '';
}

function go(scope) {
  const hash = scope ? `#scope=${encodeURIComponent(scope)}` : '#';
  if (location.hash === hash || (!scope && location.hash === '')) load(scope);
  else location.hash = hash;
}

window.addEventListener('hashchange', () => load(scopeFromHash()));

function renderCrumbs(crumbs) {
  const ids = state.scope ? state.scope.split('/') : [];
  const names = crumbs || [state.names.get('') || state.repo?.name || 'System', ...ids.map((id, i) => state.names.get(ids.slice(0, i + 1).join('/')) || id)];
  $('#crumbs').innerHTML = names.map((name, i) => {
    const scope = ids.slice(0, i).join('/');
    const last = i === names.length - 1;
    return (i ? '<span class="sep">›</span>' : '') +
      (last ? `<span class="here">${esc(name)}</span>` : `<a data-scope="${esc(scope)}">${esc(name)}</a>`);
  }).join('');
}

$('#crumbs').addEventListener('click', (e) => {
  const a = e.target.closest('a[data-scope]');
  if (a) go(a.dataset.scope);
});

async function load(scope) {
  clearTimeout(state.poll);
  state.scope = scope;
  state.selected = null;
  state.flow = null;
  renderCrumbs();
  let res;
  try {
    res = await api(`/api/map?scope=${encodeURIComponent(scope)}`);
  } catch (err) {
    return showError(err.message, scope);
  }
  if (state.scope !== scope) return;
  if (res.status === 'ready') return showMap(res.map);
  if (res.status === 'running') return watch(res.job, scope);
  start(scope, false);
}

async function start(scope, force) {
  try {
    const res = await post('/api/explore', { scope, force });
    if (state.scope !== scope) return;
    if (res.status === 'ready') return load(scope);
    watch(res.job, scope);
  } catch (err) {
    showError(err.message, scope);
  }
}

// ---------- exploration progress ----------

function showScreen(html) {
  const screen = $('#screen');
  screen.innerHTML = html;
  screen.hidden = false;
}

function watch(jobId, scope) {
  const label = renderCrumbsText();
  showScreen(`
    <div class="row"><div class="spinner"></div><h2>Exploring ${esc(label)}</h2></div>
    <div class="muted">Codex is reading the repository in a read-only sandbox. <span id="elapsed"></span></div>
    <pre class="log" id="log"></pre>
    <div class="row"><button id="cancel" type="button">Cancel</button></div>`);
  renderPanelNote('Exploration in progress. The map appears here when Codex finishes and its citations are verified.');
  $('#cancel').onclick = () => post(`/api/job/${jobId}/cancel`).catch((err) => showError(err.message, scope));
  let since = 0;
  const log = $('#log');
  const tick = async () => {
    if (state.scope !== scope) return;
    let job;
    try {
      job = await api(`/api/job/${jobId}?since=${since}`);
    } catch (err) {
      return showError(err.message, scope);
    }
    if (state.scope !== scope) return;
    since = job.next;
    const stick = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
    for (const line of job.progress) {
      const div = document.createElement('div');
      div.className = line.startsWith('$ ') ? 'cmd' : line.startsWith('› ') ? 'msg' : line.startsWith('! ') ? 'bad' : '';
      div.textContent = line;
      log.appendChild(div);
    }
    if (stick) log.scrollTop = log.scrollHeight;
    const el = $('#elapsed');
    if (el) el.textContent = `Elapsed ${fmtDuration(job.elapsed_ms)}.`;
    if (job.status === 'running') state.poll = setTimeout(tick, 1000);
    else if (job.status === 'done') load(scope);
    else if (job.status === 'cancelled') showError('Exploration cancelled.', scope);
    else showError(job.error || job.status, scope);
  };
  tick();
}

function renderCrumbsText() {
  return [...$('#crumbs').querySelectorAll('a, .here')].map((el) => el.textContent).join(' › ');
}

function showError(message, scope) {
  const parent = scope.includes('/') ? scope.slice(0, scope.lastIndexOf('/')) : '';
  showScreen(`
    <h2>Could not show this map</h2>
    <div class="error">${esc(message)}</div>
    <div class="row">
      <button class="primary" id="retry" type="button">Retry exploration</button>
      ${scope ? '<button id="up" type="button">Back up one level</button>' : ''}
    </div>`);
  renderPanelNote('');
  $('#retry').onclick = () => start(scope, true);
  if (scope) $('#up').onclick = () => go(parent);
}

function renderPanelNote(text) {
  $('#panel').innerHTML = text ? `<p class="muted">${esc(text)}</p>` : '';
}

// ---------- layout ----------

/** Longest-path layer of every node once cycles are broken; seeds the force layout. */
function layering(n, links) {
  const out = Array.from({ length: n }, () => []);
  const indeg = new Array(n).fill(0);
  for (const [u, v] of links) { out[u].push(v); indeg[v]++; }
  const mark = new Array(n).fill(0);
  const back = new Set();
  const dfs = (u) => {
    mark[u] = 1;
    for (const v of out[u]) {
      if (mark[v] === 1) back.add(`${u},${v}`);
      else if (!mark[v]) dfs(v);
    }
    mark[u] = 2;
  };
  [...Array(n).keys()].sort((a, b) => indeg[a] - indeg[b]).forEach((u) => { if (!mark[u]) dfs(u); });
  const succ = Array.from({ length: n }, () => []);
  const remaining = new Array(n).fill(0);
  for (const [u, v] of links) {
    const [a, b] = back.has(`${u},${v}`) ? [v, u] : [u, v];
    succ[a].push(b);
    remaining[b]++;
  }
  const layer = new Array(n).fill(0);
  const queue = [...Array(n).keys()].filter((i) => !remaining[i]);
  while (queue.length) {
    const u = queue.shift();
    for (const v of succ[u]) {
      layer[v] = Math.max(layer[v], layer[u] + 1);
      if (--remaining[v] === 0) queue.push(v);
    }
  }
  return layer;
}

/**
 * Deterministic force-directed layout. Seeding by layer keeps callers left of
 * callees; forces then compact dense, cyclic graphs that a strict layered
 * layout would stretch into a long strip. Rectangles are separated last.
 */
function layout(map) {
  const n = map.nodes.length;
  const index = new Map(map.nodes.map((node, i) => [node.id, i]));
  const boxes = map.nodes.map((node) => {
    const name = trunc(node.name, 30);
    return { name, w: Math.max(140, Math.min(270, name.length * 7.7 + 36)) };
  });
  const seen = new Set();
  const links = [];
  for (const e of map.edges) {
    const u = index.get(e.from);
    const v = index.get(e.to);
    if (u === undefined || v === undefined || u === v || seen.has(`${u},${v}`)) continue;
    seen.add(`${u},${v}`);
    links.push([u, v]);
  }
  const undirected = links.filter(([u, v]) => !(seen.has(`${v},${u}`) && v < u));

  const layer = layering(n, links);
  const filled = new Map();
  const x = new Float64Array(n);
  const y = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    const k = filled.get(layer[i]) || 0;
    filled.set(layer[i], k + 1);
    x[i] = layer[i] * 240;
    y[i] = ((k + 1) >> 1) * 110 * (k % 2 ? 1 : -1);
  }
  const meanX = x.reduce((s, v) => s + v, 0) / (n || 1);
  for (let i = 0; i < n; i++) x[i] -= meanX;

  // Boxes are much wider than tall, so horizontal distance counts for less.
  const SX = 0.62;
  let temp = IDEAL_EDGE;
  const fx = new Float64Array(n);
  const fy = new Float64Array(n);
  for (let it = 0; it < 600; it++) {
    fx.fill(0);
    fy.fill(0);
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        let dx = (x[i] - x[j]) * SX;
        let dy = y[i] - y[j];
        let d = Math.hypot(dx, dy);
        if (d < 1) { dx = 1; dy = 0; d = 1; }
        if (d > IDEAL_EDGE * REPULSION_RANGE) continue;
        const f = (IDEAL_EDGE * IDEAL_EDGE) / d;
        fx[i] += (dx / d) * f; fy[i] += (dy / d) * f;
        fx[j] -= (dx / d) * f; fy[j] -= (dy / d) * f;
      }
    }
    for (const [u, v] of undirected) {
      const dx = (x[v] - x[u]) * SX;
      const dy = y[v] - y[u];
      const d = Math.max(1, Math.hypot(dx, dy));
      const f = (d * d) / IDEAL_EDGE;
      fx[u] += (dx / d) * f; fy[u] += (dy / d) * f;
      fx[v] -= (dx / d) * f; fy[v] -= (dy / d) * f;
    }
    for (let i = 0; i < n; i++) {
      fx[i] -= x[i] * SX * 0.06;
      fy[i] -= y[i] * 0.06;
      const len = Math.hypot(fx[i], fy[i]);
      if (!len) continue;
      const step = Math.min(len, temp) / len;
      x[i] += (fx[i] * step) / SX;
      y[i] += fy[i] * step;
    }
    temp = Math.max(2, temp * 0.985);
  }

  for (let pass = 0; pass < 400; pass++) {
    let moved = false;
    for (let i = 0; i < n; i++) {
      for (let j = i + 1; j < n; j++) {
        const dx = x[j] - x[i];
        const dy = y[j] - y[i];
        const ox = (boxes[i].w + boxes[j].w) / 2 + MARGIN_X - Math.abs(dx);
        const oy = NODE_H + MARGIN_Y - Math.abs(dy);
        if (ox <= 0 || oy <= 0) continue;
        moved = true;
        if (ox < oy) {
          const s = ((dx >= 0 ? 1 : -1) * ox) / 2;
          x[i] -= s; x[j] += s;
        } else {
          const s = ((dy >= 0 ? 1 : -1) * oy) / 2;
          y[i] -= s; y[j] += s;
        }
      }
    }
    if (!moved) break;
  }

  const at = {};
  map.nodes.forEach((node, i) => {
    const w = boxes[i].w;
    at[node.id] = { x: x[i] - w / 2, y: y[i] - NODE_H / 2, w, h: NODE_H, cx: x[i], cy: y[i], name: boxes[i].name };
  });
  return { at };
}

/** Where the segment from box centre `c` towards `to` leaves the box. */
function clipToBox(box, c, to) {
  const dx = to[0] - c[0];
  const dy = to[1] - c[1];
  const tx = dx ? (box.w / 2 + 3) / Math.abs(dx) : Infinity;
  const ty = dy ? (box.h / 2 + 3) / Math.abs(dy) : Infinity;
  const t = Math.min(tx, ty, 1);
  return [c[0] + dx * t, c[1] + dy * t];
}

/** Quadratic curve between two boxes; `offset` bends parallel edges apart. */
function edgePath(L, e, offset) {
  const a = L.at[e.from];
  const b = L.at[e.to];
  const ca = [a.cx, a.cy];
  const cb = [b.cx, b.cy];
  // Bend relative to a canonical direction so A→B and B→A separate.
  const [p, q] = e.from < e.to ? [ca, cb] : [cb, ca];
  const len = Math.hypot(q[0] - p[0], q[1] - p[1]) || 1;
  const normal = [-(q[1] - p[1]) / len, (q[0] - p[0]) / len];
  const c = [(ca[0] + cb[0]) / 2 + normal[0] * offset, (ca[1] + cb[1]) / 2 + normal[1] * offset];
  const p0 = clipToBox(a, ca, c);
  const p3 = clipToBox(b, cb, c);
  const mid = [0, 1].map((i) => 0.25 * p0[i] + 0.5 * c[i] + 0.25 * p3[i]);
  return { d: `M${p0} Q${c} ${p3}`, mid };
}

// ---------- graph rendering ----------

function layoutKey() {
  return `code-explore:layout:${state.repo?.root}@${state.repo?.commit || 'no-commit'}:${state.scope}`;
}

/** Positions the user dragged, keyed by node id (box centres). */
function savedPositions() {
  try {
    return JSON.parse(localStorage.getItem(layoutKey())) || null;
  } catch {
    return null;
  }
}

function savePositions() {
  const at = state.layout.at;
  const positions = Object.fromEntries(Object.entries(at).map(([id, p]) => [id, [Math.round(p.cx), Math.round(p.cy)]]));
  try {
    localStorage.setItem(layoutKey(), JSON.stringify(positions));
  } catch {
    // Storage full or disabled: the drag still applies for this view.
  }
  state.customLayout = true;
}

function moveNode(p, cx, cy) {
  p.cx = cx;
  p.cy = cy;
  p.x = cx - p.w / 2;
  p.y = cy - p.h / 2;
}

function computeLayout(map) {
  const L = layout(map);
  const saved = savedPositions();
  state.customLayout = false;
  for (const [id, [cx, cy]] of Object.entries(saved || {})) {
    if (L.at[id] && Number.isFinite(cx) && Number.isFinite(cy)) {
      moveNode(L.at[id], cx, cy);
      state.customLayout = true;
    }
  }
  return L;
}

function showMap(map) {
  state.map = map;
  state.layout = computeLayout(map);
  const crumbs = map.meta?.crumbs;
  if (crumbs) {
    const ids = state.scope ? state.scope.split('/') : [];
    crumbs.forEach((name, i) => state.names.set(ids.slice(0, i).join('/'), name));
  }
  for (const node of map.nodes) state.names.set(childScope(node.id), node.name);
  renderCrumbs(crumbs);
  $('#screen').hidden = true;
  renderGraph();
  fit();
  renderPanel();
}

function renderGraph() {
  renderNodes();
  renderEdges();
}

/** Edge geometry follows node positions; called again while a node is dragged. */
function renderEdges() {
  const { map, layout: L } = state;
  const pairCount = new Map();
  const pairSeen = new Map();
  const pairKey = (e) => [e.from, e.to].sort().join('\u0000');
  map.edges.forEach((e) => pairCount.set(pairKey(e), (pairCount.get(pairKey(e)) || 0) + 1));

  state.edgeGeom = map.edges.map((e) => {
    if (e.from === e.to) {
      const a = L.at[e.from];
      const x = a.x + a.w - 24;
      return { d: `M${x},${a.y} C${x - 10},${a.y - 46} ${x + 40},${a.y - 46} ${a.x + a.w},${a.y + 12}`, mid: [x + 14, a.y - 36] };
    }
    const key = pairKey(e);
    const k = pairSeen.get(key) || 0;
    pairSeen.set(key, k + 1);
    return edgePath(L, e, (k - (pairCount.get(key) - 1) / 2) * 36);
  });

  $('#edges').innerHTML = map.edges.map((e, i) => {
    const g = state.edgeGeom[i];
    return `<g class="edge k-${esc(e.kind)}" data-edge="${i}">
      <path class="line" d="${g.d}" marker-end="url(#arrow)"></path>
      <path class="hit" d="${g.d}"><title>${esc(`${nodeById(e.from)?.name} → ${nodeById(e.to)?.name}: ${e.label}`)}</title></path>
      <text class="label" x="${g.mid[0]}" y="${g.mid[1] - 5}" text-anchor="middle">${esc(trunc(e.label, 30))}</text>
    </g>`;
  }).join('');
  applyHighlight();
}

function nodeTransform(p) {
  return `translate(${p.x},${p.y})`;
}

function renderNodes() {
  const { map, layout: L } = state;
  $('#nodes').innerHTML = map.nodes.map((node) => {
    const p = L.at[node.id];
    const drill = node.paths.length > 0;
    return `<g class="node ${esc(node.kind)}" data-node="${esc(node.id)}" transform="${nodeTransform(p)}" style="--kc:${kindColor(node.kind)}">
      <title>${esc(node.name)} (drag to move${drill ? ', double-click to explore inside' : ''})</title>
      <rect class="box" width="${p.w}" height="${p.h}" rx="9"></rect>
      <rect class="band" width="5" height="${p.h - 16}" x="0" y="8" rx="2"></rect>
      <text class="name" x="16" y="23">${esc(p.name)}</text>
      <text class="kind" x="16" y="41">${esc(node.kind)}</text>
      ${drill ? `<text class="drill" x="${p.w - 12}" y="41" text-anchor="end">${node.explorable ? 'explore ›' : '›'}</text>` : ''}
    </g>`;
  }).join('');
}

function applyHighlight() {
  const { map, selected, flow } = state;
  const nodes = document.querySelectorAll('#nodes .node');
  const edges = document.querySelectorAll('#edges .edge');
  const overlay = $('#overlay');
  overlay.innerHTML = '';
  nodes.forEach((el) => el.classList.remove('dim', 'selected'));
  edges.forEach((el) => {
    el.classList.remove('dim', 'hi');
    $('path.line', el).setAttribute('marker-end', 'url(#arrow)');
  });
  const hiEdge = (i) => {
    edges[i].classList.add('hi');
    $('path.line', edges[i]).setAttribute('marker-end', 'url(#arrow-hi)');
  };

  if (flow !== null && map.flows[flow]) {
    const steps = map.flows[flow].steps;
    const litNodes = new Set(steps.flatMap((s) => [s.from, s.to]));
    const badges = new Map(); // edge index or ghost key -> { mid, numbers }
    steps.forEach((s, n) => {
      let i = map.edges.findIndex((e) => e.from === s.from && e.to === s.to);
      if (i < 0) i = map.edges.findIndex((e) => e.from === s.to && e.to === s.from);
      let key, mid;
      if (i >= 0) {
        key = `e${i}`;
        mid = state.edgeGeom[i].mid;
      } else {
        // The flow step has no matching edge: draw a dashed guide for it.
        key = `g${s.from}>${s.to}`;
        const g = s.from === s.to ? null : edgePath(state.layout, s, 0);
        if (g && !badges.has(key)) overlay.insertAdjacentHTML('beforeend', `<path class="ghost" d="${g.d}" marker-end="url(#arrow-hi)"></path>`);
        mid = g ? g.mid : [state.layout.at[s.from].x, state.layout.at[s.from].y];
      }
      if (!badges.has(key)) badges.set(key, { mid, numbers: [], edge: i });
      badges.get(key).numbers.push(n + 1);
    });
    nodes.forEach((el) => { if (!litNodes.has(el.dataset.node)) el.classList.add('dim'); });
    const litEdges = new Set([...badges.values()].map((b) => b.edge).filter((i) => i >= 0));
    edges.forEach((el, i) => { if (litEdges.has(i)) hiEdge(i); else el.classList.add('dim'); });
    for (const { mid, numbers } of badges.values()) {
      const text = numbers.join(',');
      const r = Math.max(10, text.length * 3.6 + 5);
      overlay.insertAdjacentHTML('beforeend', `<g class="step-badge" transform="translate(${mid[0]},${mid[1] + 9})">
        <rect x="${-r}" y="-10" width="${2 * r}" height="20" rx="10" style="fill:var(--accent)"></rect>
        <text text-anchor="middle" y="4">${esc(text)}</text></g>`);
    }
    return;
  }

  if (selected?.type === 'node') {
    const id = selected.id;
    const near = new Set([id]);
    map.edges.forEach((e) => { if (e.from === id || e.to === id) { near.add(e.from); near.add(e.to); } });
    nodes.forEach((el) => {
      if (el.dataset.node === id) el.classList.add('selected');
      else if (!near.has(el.dataset.node)) el.classList.add('dim');
    });
    edges.forEach((el, i) => {
      const e = map.edges[i];
      if (e.from === id || e.to === id) hiEdge(i); else el.classList.add('dim');
    });
  } else if (selected?.type === 'edge') {
    const e = map.edges[selected.index];
    nodes.forEach((el) => {
      if (el.dataset.node === e.from || el.dataset.node === e.to) el.classList.add('selected');
      else el.classList.add('dim');
    });
    edges.forEach((el, i) => { if (i === selected.index) hiEdge(i); else el.classList.add('dim'); });
  }
}

// ---------- pan & zoom ----------

const svg = $('#graph');

function applyView() {
  const { x, y, k } = state.view;
  $('#viewport').setAttribute('transform', `translate(${x},${y}) scale(${k})`);
}

function fit() {
  const box = $('#viewport').getBBox();
  const r = svg.getBoundingClientRect();
  if (!box.width || !r.width) return;
  const pad = 48;
  const k = Math.min((r.width - 2 * pad) / box.width, (r.height - 2 * pad) / box.height, 1.25);
  state.view = { k, x: (r.width - box.width * k) / 2 - box.x * k, y: (r.height - box.height * k) / 2 - box.y * k };
  applyView();
}

svg.addEventListener('wheel', (e) => {
  e.preventDefault();
  const r = svg.getBoundingClientRect();
  const mx = e.clientX - r.left;
  const my = e.clientY - r.top;
  const v = state.view;
  const k = Math.min(4, Math.max(0.12, v.k * Math.exp(-e.deltaY * 0.0015)));
  v.x = mx - (mx - v.x) * (k / v.k);
  v.y = my - (my - v.y) * (k / v.k);
  v.k = k;
  applyView();
}, { passive: false });

// Dragging a node moves it (edges follow); dragging the background pans.
let drag = null;
let edgeFrame = 0;
let suppressClick = false; // the click that ends a drag must not select
svg.addEventListener('pointerdown', (e) => {
  if (e.button !== 0) return;
  suppressClick = false;
  const nodeEl = e.target.closest('[data-node]');
  const p = nodeEl && state.layout?.at[nodeEl.dataset.node];
  drag = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y, moved: false };
  if (p) Object.assign(drag, { el: nodeEl, p, cx: p.cx, cy: p.cy });
});
window.addEventListener('pointermove', (e) => {
  if (!drag) return;
  if (!(e.buttons & 1)) return endDrag(); // button released outside the page
  const dx = e.clientX - drag.x;
  const dy = e.clientY - drag.y;
  if (!drag.moved && Math.hypot(dx, dy) < 4) return;
  if (!drag.moved) svg.setPointerCapture?.(e.pointerId);
  drag.moved = true;
  if (drag.p) {
    if (!drag.el.classList.contains('dragging')) {
      drag.el.classList.add('dragging');
      drag.el.parentNode.appendChild(drag.el); // draw above the other boxes
    }
    moveNode(drag.p, drag.cx + dx / state.view.k, drag.cy + dy / state.view.k);
    drag.el.setAttribute('transform', nodeTransform(drag.p));
    if (!edgeFrame) edgeFrame = requestAnimationFrame(() => { edgeFrame = 0; renderEdges(); });
    return;
  }
  svg.classList.add('panning');
  state.view.x = drag.vx + dx;
  state.view.y = drag.vy + dy;
  applyView();
});
function endDrag() {
  svg.classList.remove('panning');
  if (drag?.p && drag.moved) {
    drag.el.classList.remove('dragging');
    savePositions();
    if (!state.selected && state.flow === null) renderPanel(); // shows “Reset layout”
  }
  suppressClick = !!drag?.moved;
  drag = null;
}
window.addEventListener('pointerup', endDrag);
window.addEventListener('pointercancel', endDrag);

svg.addEventListener('click', (e) => {
  if (suppressClick) {
    suppressClick = false;
    return;
  }
  const nodeEl = e.target.closest('[data-node]');
  const edgeEl = e.target.closest('[data-edge]');
  state.flow = null;
  if (nodeEl) state.selected = { type: 'node', id: nodeEl.dataset.node };
  else if (edgeEl) state.selected = { type: 'edge', index: Number(edgeEl.dataset.edge) };
  else state.selected = null;
  applyHighlight();
  renderPanel();
});

svg.addEventListener('dblclick', (e) => {
  const nodeEl = e.target.closest('[data-node]');
  const node = nodeEl && nodeById(nodeEl.dataset.node);
  if (node?.paths.length) go(childScope(node.id));
});

$('#fit').onclick = fit;
$('#labels').onchange = (e) => svg.classList.toggle('labels-off', !e.target.checked);
window.addEventListener('resize', () => { if (state.map && $('#screen').hidden) fit(); });

// ---------- side panel ----------

function evidenceList(items) {
  if (!items.length) return '<p class="muted">No citations given.</p>';
  return '<ul class="list">' + items.map((ev) => `<li class="ev">
      <span class="mark ${ev.verified ? 'ok' : 'bad'}" title="${ev.verified ? 'Verified: file and line exist' : esc(`Unverified: ${ev.problem || 'not checked'}`)}">${ev.verified ? '✓' : '!'}</span>
      <span><a data-src="${esc(ev.path)}" data-line="${ev.line}">${esc(ev.path)}:${ev.line}</a>
      ${ev.note ? `<span class="note"> ${esc(ev.note)}</span>` : ''}
      ${ev.verified ? '' : `<div class="note warn">${esc(ev.problem || 'unverified')}</div>`}</span>
    </li>`).join('') + '</ul>';
}

function edgeItem(e, i, perspective) {
  const other = perspective === e.from ? `→ ${esc(nodeById(e.to)?.name)}` : perspective === e.to ? `← ${esc(nodeById(e.from)?.name)}` : `${esc(nodeById(e.from)?.name)} → ${esc(nodeById(e.to)?.name)}`;
  return `<li class="click" data-sel-edge="${i}"><b>${other}</b> <span class="kind-chip">${esc(e.kind)}</span><div class="muted">${esc(e.label)}</div></li>`;
}

/** Re-render the side panel; scroll back to the top when the subject changes. */
function renderPanel() {
  const sel = state.selected;
  const subject = `${state.scope}|${sel ? `${sel.type}:${sel.id ?? sel.index}` : 'map'}`;
  renderPanelContent();
  if (subject !== state.panelSubject) $('#panel').scrollTop = 0;
  state.panelSubject = subject;
  if (state.tab === 'ask') renderAskContext();
}

function renderPanelContent() {
  const { map, selected } = state;
  const panel = $('#panel');
  if (selected?.type === 'node') {
    const node = nodeById(selected.id);
    const rel = map.edges.map((e, i) => [e, i]).filter(([e]) => e.from === node.id || e.to === node.id);
    panel.innerHTML = `
      <div class="actions" style="margin:0 0 12px"><button data-act="back" type="button">‹ Map overview</button></div>
      <h2>${esc(node.name)}</h2>
      <span class="kind-chip" style="--kc:${kindColor(node.kind)}">${esc(node.kind)}</span>
      ${md(node.summary)}
      <div class="actions">
        ${node.paths.length ? '<button class="primary" data-act="drill" type="button">Explore inside ›</button>' : ''}
        <button data-act="ask" type="button">Ask about this</button>
      </div>
      <h3>Paths</h3>
      ${node.paths.length ? '<ul class="list">' + node.paths.map((p) => `<li><span class="path-link" data-src="${esc(p)}">${esc(p)}</span></li>`).join('') + '</ul>' : '<p class="muted">Not implemented in this repository.</p>'}
      <h3>Interactions (${rel.length})</h3>
      ${rel.length ? '<ul class="list">' + rel.map(([e, i]) => edgeItem(e, i, node.id)).join('') + '</ul>' : '<p class="muted">None recorded.</p>'}
      <h3>Evidence</h3>
      ${evidenceList(node.evidence)}`;
    return;
  }
  if (selected?.type === 'edge') {
    const e = map.edges[selected.index];
    panel.innerHTML = `
      <div class="actions" style="margin:0 0 12px"><button data-act="back" type="button">‹ Map overview</button></div>
      <h2>${esc(nodeById(e.from)?.name)} → ${esc(nodeById(e.to)?.name)}</h2>
      <span class="kind-chip">${esc(e.kind)}</span>
      ${md(e.label)}
      <div class="actions">
        <button data-sel-node="${esc(e.from)}" type="button">${esc(trunc(nodeById(e.from)?.name, 24))}</button>
        <button data-sel-node="${esc(e.to)}" type="button">${esc(trunc(nodeById(e.to)?.name, 24))}</button>
      </div>
      <h3>Evidence</h3>
      ${evidenceList(e.evidence)}`;
    return;
  }

  const meta = map.meta;
  const allEvidence = [...map.nodes.flatMap((n) => n.evidence), ...map.edges.flatMap((e) => e.evidence)];
  const verified = allEvidence.filter((ev) => ev.verified).length;
  const flow = state.flow !== null ? map.flows[state.flow] : null;
  panel.innerHTML = `
    <h2>${esc(map.title)}</h2>
    ${md(map.summary)}
    <p class="meta">
      ${meta?.commit ? `Commit <code>${esc(meta.commit.slice(0, 10))}</code>${meta.dirty ? ' <span class="warn">(uncommitted changes)</span>' : ''} · ` : ''}
      ${meta ? `${new Date(meta.generated_at * 1000).toLocaleString()} · ${fmtDuration(meta.duration_ms)} · ${meta.commands} commands · ${fmtTokens(meta.input_tokens)} in / ${fmtTokens(meta.output_tokens)} out tokens` : ''}
      ${meta?.model ? ` · ${esc(meta.model)}` : ''}
    </p>
    <p class="meta">${map.nodes.length} nodes · ${map.edges.length} interactions ·
      <span class="${verified === allEvidence.length ? '' : 'warn'}">${verified}/${allEvidence.length} citations verified</span></p>
    <div class="actions"><button data-act="reexplore" type="button">Re-explore</button>
      ${state.customLayout ? '<button data-act="reset-layout" type="button" title="Discard dragged positions">Reset layout</button>' : ''}</div>
    <h3>Data flows</h3>
    ${map.flows.length ? '<ul class="list">' + map.flows.map((f, i) => `<li class="click ${i === state.flow ? 'active' : ''}" data-flow="${i}">
        <b>${esc(f.name)}</b><div class="muted">${esc(f.description)}</div></li>`).join('') + '</ul>' : '<p class="muted">None identified.</p>'}
    ${flow ? `<h3>Steps: ${esc(flow.name)}</h3><ul class="list">` + flow.steps.map((s, n) => {
      let i = map.edges.findIndex((e) => e.from === s.from && e.to === s.to);
      if (i < 0) i = map.edges.findIndex((e) => e.from === s.to && e.to === s.from);
      return `<li class="${i >= 0 ? 'click' : ''}" ${i >= 0 ? `data-sel-edge="${i}"` : ''}><div class="step"><span class="step-no">${n + 1}</span>
        <span><b>${esc(nodeById(s.from)?.name)} → ${esc(nodeById(s.to)?.name)}</b><div class="muted">${esc(s.action)}</div></span></div></li>`;
    }).join('') + '</ul>' : ''}
    <h3>Components</h3>
    <ul class="list">${map.nodes.map((n) => `<li class="click" data-sel-node="${esc(n.id)}"><span class="swatch" style="--kc:${kindColor(n.kind)}"></span><b>${esc(n.name)}</b>
      <span class="muted"> ${esc(n.kind)}</span>${n.paths.length ? ` <a class="path-link" data-drill="${esc(n.id)}">explore ›</a>` : ''}</li>`).join('')}</ul>
    ${map.diagnostics.length ? `<h3>Diagnostics</h3><details><summary>${map.diagnostics.length} issues in the agent's answer</summary>
      <ul>${map.diagnostics.map((d) => `<li class="meta">${esc(d)}</li>`).join('')}</ul></details>` : ''}
    <h3>How to read</h3>
    <p class="meta">Click a box or arrow for details and evidence. Double-click a box, or use “Explore inside”, to zoom in one level.
      Drag a box to move it; positions are kept per map in this browser.
      Arrows point the way requests or data travel. Dashed boxes are external systems or actors.</p>
    <div class="legend">
      ${legendLine('call / http / rpc', '')}${legendLine('event / queue', '7 4')}
      ${legendLine('read / write / file', '2 3')}${legendLine('deploy / config / import', '12 4 2 4')}
    </div>`;
}

function legendLine(text, dash) {
  return `<span><svg width="34" height="8"><line x1="0" y1="4" x2="34" y2="4" stroke="var(--edge)" stroke-width="1.6" stroke-dasharray="${dash}"></line></svg>${esc(text)}</span>`;
}

$('#panel').addEventListener('click', (e) => {
  const t = e.target.closest('[data-src], [data-drill], [data-sel-node], [data-sel-edge], [data-flow], [data-act]');
  if (!t) return;
  if (t.dataset.src !== undefined) return openSource(t.dataset.src, Number(t.dataset.line) || 0);
  if (t.dataset.drill !== undefined) return go(childScope(t.dataset.drill));
  if (t.dataset.selNode !== undefined) { state.flow = null; state.selected = { type: 'node', id: t.dataset.selNode }; }
  else if (t.dataset.selEdge !== undefined) { state.flow = null; state.selected = { type: 'edge', index: Number(t.dataset.selEdge) }; }
  else if (t.dataset.flow !== undefined) { const i = Number(t.dataset.flow); state.selected = null; state.flow = state.flow === i ? null : i; }
  else if (t.dataset.act === 'back') state.selected = null;
  else if (t.dataset.act === 'drill') return go(childScope(state.selected.id));
  else if (t.dataset.act === 'ask') return switchTab('ask');
  else if (t.dataset.act === 'reexplore') {
    localStorage.removeItem(layoutKey()); // node ids may mean something else in the new map
    return start(state.scope, true);
  } else if (t.dataset.act === 'reset-layout') {
    localStorage.removeItem(layoutKey());
    state.layout = computeLayout(state.map);
    renderGraph();
    fit();
  }
  applyHighlight();
  renderPanel();
});

// ---------- highlighting ----------

const HL_BY_EXT = {
  rs: 'rust', py: 'python', pyi: 'python', js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'javascript',
  ts: 'typescript', tsx: 'typescript', mts: 'typescript', cts: 'typescript', go: 'go', java: 'java', kt: 'kotlin',
  swift: 'swift', c: 'c', h: 'c', cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hh: 'cpp', cs: 'csharp', rb: 'ruby',
  php: 'php', pl: 'perl', lua: 'lua', r: 'r', sh: 'bash', bash: 'bash', zsh: 'bash', ps1: 'powershell',
  psm1: 'powershell', psd1: 'powershell', psrc: 'powershell', yaml: 'yaml', yml: 'yaml', json: 'json', jsonl: 'json',
  toml: 'ini', ini: 'ini', cfg: 'ini', service: 'ini', timer: 'ini', socket: 'ini', path: 'ini', md: 'markdown',
  markdown: 'markdown', html: 'xml', htm: 'xml', xml: 'xml', svg: 'xml', css: 'css', scss: 'scss', less: 'less',
  sql: 'sql', graphql: 'graphql', diff: 'diff', patch: 'diff', mk: 'makefile',
};
const MAX_HIGHLIGHT_CHARS = 600000;

function hlLanguage(path) {
  const name = path.split('/').pop();
  let lang = null;
  if (/^Dockerfile|\.dockerfile$/i.test(name)) lang = 'dockerfile';
  else if (/^(GNU)?makefile$/i.test(name)) lang = 'makefile';
  else if (/nginx.*\.conf$|\.nginx$/i.test(name)) lang = 'nginx';
  else if (name.includes('.')) lang = HL_BY_EXT[name.split('.').pop().toLowerCase()] || null;
  return lang && window.hljs?.getLanguage(lang) ? lang : null;
}

/** Split highlighted HTML into lines, closing and reopening spans that cross a line break. */
function splitHtmlLines(html) {
  const lines = [];
  const open = [];
  let cur = '';
  const re = /(<span[^>]*>)|(<\/span>)|(\n)|([^<\n]+)/g;
  let m;
  while ((m = re.exec(html))) {
    if (m[1]) { open.push(m[1]); cur += m[1]; }
    else if (m[2]) { open.pop(); cur += m[2]; }
    else if (m[3]) { lines.push(cur + '</span>'.repeat(open.length)); cur = open.join(''); }
    else cur += m[4];
  }
  lines.push(cur + '</span>'.repeat(open.length));
  return lines;
}

/** Per-line HTML for a file: highlighted when the language is known, escaped text otherwise. */
function highlightLines(text, path) {
  const lang = text.length <= MAX_HIGHLIGHT_CHARS ? hlLanguage(path) : null;
  if (lang) {
    try {
      return splitHtmlLines(hljs.highlight(text, { language: lang, ignoreIllegals: true }).value);
    } catch {
      // fall through to plain text
    }
  }
  return text.split('\n').map(esc);
}

function highlightBlock(code, lang) {
  const known = lang && window.hljs?.getLanguage(lang) ? lang : null;
  if (!known) return esc(code);
  try {
    return hljs.highlight(code, { language: known, ignoreIllegals: true }).value;
  } catch {
    return esc(code);
  }
}

// ---------- markdown (answers, hover docs) ----------

const LOC_RE = /^((?:[\w.@-]+\/)*[\w.@-]+\.[A-Za-z0-9]+):(\d+)(?:-(\d+))?$/;
const LOC_IN_TEXT_RE = /(^|[\s(\[“"'])((?:[\w.@-]+\/)*[\w.@-]+\.[A-Za-z0-9]+):(\d+)(?:-(\d+))?/g;

function locLink(path, line, label) {
  return `<a class="loc" data-src="${esc(path)}" data-line="${line}">${label}</a>`;
}

function mdInline(text) {
  // Code spans become placeholders so emphasis can wrap them (`**`x`**`).
  const codes = [];
  const masked = String(text).replace(/`([^`\n]+)`/g, (_, code) => {
    const loc = code.match(LOC_RE);
    codes.push(loc ? locLink(loc[1], loc[2], `<code>${esc(code)}</code>`) : `<code>${esc(code)}</code>`);
    return `\u0001${codes.length - 1}\u0002`;
  });
  return esc(masked)
    .replace(/\*\*([^*]+)\*\*/g, '<b>$1</b>')
    .replace(/(^|[^*\w])\*([^*\s][^*]*)\*(?!\w)/g, '$1<i>$2</i>')
    .replace(/\[([^\]]+)\]\([^)\s]+\)/g, '$1')
    .replace(LOC_IN_TEXT_RE, (all, pre, path, line, end) => pre + locLink(path, line, `${path}:${line}${end ? `-${end}` : ''}`))
    .replace(/\n/g, '<br>')
    .replace(/\u0001(\d+)\u0002/g, (_, i) => codes[Number(i)]);
}

/** Markdown subset: fenced code, headings, lists, paragraphs, inline code/bold/italic, `path:line` links. */
function mdBlock(text) {
  const out = [];
  const lines = String(text ?? '').replace(/\r/g, '').split('\n');
  let para = [];
  let list = null;
  const flushPara = () => {
    if (para.length) out.push(`<p>${mdInline(para.join('\n'))}</p>`);
    para = [];
  };
  const flushList = () => {
    if (list) {
      const start = list.type === 'ol' && list.start !== 1 ? ` start="${list.start}"` : '';
      out.push(`<${list.type}${start}>${list.items.map((it) => `<li>${mdInline(it)}</li>`).join('')}</${list.type}>`);
    }
    list = null;
  };
  const ITEM_RE = /^\s*([-*+]|\d+[.)])\s+(.*)/;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^\s*```\s*([\w+#.-]*)/);
    if (fence) {
      flushPara(); flushList();
      const code = [];
      for (i++; i < lines.length && !/^\s*```/.test(lines[i]); i++) code.push(lines[i]);
      out.push(`<pre class="md-code hljs"><code>${highlightBlock(code.join('\n'), fence[1].toLowerCase())}</code></pre>`);
      continue;
    }
    const heading = line.match(/^#{1,6}\s+(.*)/);
    if (heading) { flushPara(); flushList(); out.push(`<h4 class="md-h">${mdInline(heading[1])}</h4>`); continue; }
    const item = line.match(ITEM_RE);
    if (item) {
      flushPara();
      const type = /\d/.test(item[1]) ? 'ol' : 'ul';
      if (list?.type !== type) { flushList(); list = { type, items: [], start: parseInt(item[1], 10) || 1 }; }
      list.items.push(item[2]);
      continue;
    }
    if (!line.trim()) {
      flushPara();
      // A blank line between items keeps the list (and its numbering) going.
      const next = lines.slice(i + 1).find((l) => l.trim());
      const nextItem = next?.match(ITEM_RE);
      if (!list || !nextItem || (/\d/.test(nextItem[1]) ? 'ol' : 'ul') !== list.type) flushList();
      continue;
    }
    if (list && /^\s{2,}\S/.test(line)) { list.items[list.items.length - 1] += ` ${line.trim()}`; continue; }
    flushList();
    para.push(line);
  }
  flushPara(); flushList();
  return `<div class="md">${out.join('')}</div>`;
}

// ---------- source viewer (L3) with navigation ----------

const viewer = { path: null, line: 0, history: [], index: -1, lsp: null, lines: [] };

function updateHistoryButtons() {
  $('#src-back').disabled = viewer.index <= 0;
  $('#src-fwd').disabled = viewer.index >= viewer.history.length - 1;
}

/** Open a file (or directory) at a 1-based line. History entries are pushed unless navigating it. */
async function openSource(path, line, { push = true } = {}) {
  if ($('#modal').hidden) { viewer.history = []; viewer.index = -1; }
  if (push) {
    viewer.history = viewer.history.slice(0, viewer.index + 1);
    viewer.history.push({ path, line });
    viewer.index = viewer.history.length - 1;
  }
  updateHistoryButtons();
  viewer.path = path;
  viewer.line = line;
  $('#modal').hidden = false;
  $('#sym').hidden = true;
  const body = $('#src-body');
  const parts = path === '.' ? [] : path.split('/');
  $('#src-path').innerHTML = [`<a data-open=".">${esc(state.repo?.name || '.')}</a>`, ...parts.map((p, i) =>
    i === parts.length - 1 ? esc(p) : `<a data-open="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</a>`)].join(' / ') + (line ? `:${line}` : '');
  $('#src-lsp').hidden = true;
  body.innerHTML = '<p class="muted" style="padding:16px">Loading…</p>';
  let res;
  try {
    res = await api(`/api/source?path=${encodeURIComponent(path)}`);
  } catch (err) {
    body.innerHTML = `<div class="src-error">${esc(err.message)}</div>`;
    return;
  }
  if (viewer.path !== path) return;
  if (res.dir) {
    viewer.lines = [];
    const base = res.path === '.' ? '' : `${res.path}/`;
    body.innerHTML = '<ul class="dir-list">' + res.entries.map((e) =>
      `<li><a data-open="${esc(base + e.name)}">${esc(e.name)}${e.dir ? '/' : ''}</a></li>`).join('') + '</ul>';
    return;
  }
  const text = res.text.endsWith('\n') ? res.text.slice(0, -1) : res.text;
  viewer.lines = text.split('\n');
  const html = highlightLines(text, path);
  body.innerHTML = `<div class="src-hint">Click a name for its definition, documentation and references · Ctrl/⌘-click jumps to the definition · Alt+←/→ history</div>` +
    html.map((h, i) => `<div class="src-line${i + 1 === line ? ' target' : ''}" data-line="${i + 1}" id="L${i + 1}"><span class="no">${i + 1}</span><span class="code">${h || ' '}</span></div>`).join('');
  const target = line && document.getElementById(`L${line}`);
  if (target) target.scrollIntoView({ block: 'center' });
  else body.scrollTop = 0;
  refreshLspStatus(path, true);
}

async function refreshLspStatus(path, start) {
  let status;
  try {
    status = await api(`/api/lsp/status?path=${encodeURIComponent(path)}${start ? '&start=true' : ''}`);
  } catch {
    status = { state: 'none' };
  }
  if (viewer.path !== path) return;
  viewer.lsp = status;
  const chip = $('#src-lsp');
  // The server's executable, not wrapper words (`npx -y -p pyright@1 pyright-langserver --stdio`).
  const words = status.server?.split(/\s+/).filter((w) => !w.startsWith('-')) || [];
  const server = (words.find((w) => /langserver|language-server|analyzer|pylsp|gopls|clangd/.test(w)) || words[0] || '').split('/').pop();
  const label = {
    none: 'text search',
    idle: `${server}: starting…`,
    starting: `${server}: starting…`,
    indexing: `${server}: indexing…`,
    ready: `${server}`,
    failed: `${server}: unavailable`,
  }[status.state];
  chip.className = `lsp-chip ${status.state}`;
  chip.textContent = label;
  chip.title = status.state === 'none'
    ? 'No language server for this file type: navigation uses whole-word text search'
    : status.error || status.server;
  chip.hidden = false;
  if (['idle', 'starting', 'indexing'].includes(status.state)) setTimeout(() => refreshLspStatus(path, false), 1500);
}

/** The identifier under a click point inside a `.code` element, with its UTF-16 column. */
function wordAt(codeEl, x, y) {
  let node, offset;
  if (document.caretPositionFromPoint) {
    const pos = document.caretPositionFromPoint(x, y);
    node = pos?.offsetNode; offset = pos?.offset;
  } else if (document.caretRangeFromPoint) {
    const range = document.caretRangeFromPoint(x, y);
    node = range?.startContainer; offset = range?.startOffset;
  }
  if (!node || !codeEl.contains(node) || node.nodeType !== Node.TEXT_NODE) return null;
  let column = 0;
  const walker = document.createTreeWalker(codeEl, NodeFilter.SHOW_TEXT);
  for (let t = walker.nextNode(); t && t !== node; t = walker.nextNode()) column += t.nodeValue.length;
  column += offset;
  const text = codeEl.textContent;
  const isWord = (c) => /[\w$]/.test(c || '');
  let start = column;
  let end = column;
  while (start > 0 && isWord(text[start - 1])) start--;
  while (end < text.length && isWord(text[end])) end++;
  if (start === end) return null;
  const word = text.slice(start, end);
  return /^\d+$/.test(word) ? null : { word, start };
}

function locItems(locations, { group = false } = {}) {
  if (!locations.length) return '<p class="muted">None found.</p>';
  const item = (l) => l.path
    ? `<li data-go="${esc(l.path)}" data-line="${l.line}"><div class="where">${esc(group ? `${l.line}` : `${l.display || l.path}:${l.line}`)}</div>${l.preview ? `<div class="preview">${esc(l.preview)}</div>` : ''}</li>`
    : `<li class="external" title="Outside the repository"><div class="where">${esc(l.display)}:${l.line}</div></li>`;
  if (!group) return `<ul class="loc-list">${locations.map(item).join('')}</ul>`;
  const byFile = new Map();
  for (const l of locations) {
    const key = l.path ?? l.display;
    if (!byFile.has(key)) byFile.set(key, []);
    byFile.get(key).push(l);
  }
  return [...byFile].map(([file, ls]) => `<div class="loc-file">${esc(file)} <span class="muted">(${ls.length})</span></div><ul class="loc-list">${ls.map(item).join('')}</ul>`).join('');
}

/** Symbol pane: LSP hover/definitions/references, or whole-word text matches without a server. */
async function showSymbol(sym, { jump = false } = {}) {
  const pane = $('#sym');
  pane.hidden = false;
  const head = (extra = '') => `<div class="sym-head"><code>${esc(sym.word)}</code><span class="spacer"></span>${extra}<button type="button" data-sym-close title="Close">✕</button></div>`;
  const usesLsp = viewer.lsp && viewer.lsp.state !== 'none' && viewer.lsp.state !== 'failed';
  if (!usesLsp) {
    pane.innerHTML = head() + `<p class="muted small">${viewer.lsp?.state === 'failed' ? `Language server unavailable (${esc(viewer.lsp.error || '')}).` : 'No language server for this file type.'} Showing whole-word text matches.</p><div id="sym-text"><p class="muted">Searching…</p></div>`;
    return textSearch(sym.word);
  }
  pane.innerHTML = head() + '<p class="muted">Asking the language server…</p>';
  let info;
  try {
    info = await post('/api/lsp/symbol', { path: sym.path, line: sym.line, character: sym.start });
  } catch (err) {
    pane.innerHTML = head() + `<p class="src-error" style="padding:0">${esc(err.message)}</p><div id="sym-text"><p class="muted">Searching text instead…</p></div>`;
    return textSearch(sym.word);
  }
  const inRepo = info.definitions.filter((d) => d.path);
  if (jump && inRepo.length === 1) return openSource(inRepo[0].path, inRepo[0].line);
  pane.innerHTML = head() +
    (info.hover ? `<div class="sym-hover">${mdBlock(info.hover)}</div>` : '') +
    `<h3>Definition${info.definitions.length === 1 ? '' : 's'}</h3>${locItems(info.definitions)}` +
    `<div class="actions"><button type="button" data-refs>Find references</button><button type="button" data-text>Text search</button></div>` +
    '<div id="sym-refs"></div><div id="sym-text"></div>';
  pane.querySelector('[data-refs]').onclick = async (e) => {
    e.target.disabled = true;
    const box = $('#sym-refs');
    box.innerHTML = '<p class="muted">Finding references…</p>';
    try {
      const { references } = await post('/api/lsp/references', { path: sym.path, line: sym.line, character: sym.start });
      box.innerHTML = `<h3>References (${references.length})</h3>${locItems(references, { group: true })}`;
    } catch (err) {
      box.innerHTML = `<p class="src-error" style="padding:0">${esc(err.message)}</p>`;
    }
  };
  pane.querySelector('[data-text]').onclick = (e) => { e.target.disabled = true; textSearch(sym.word); };
}

async function textSearch(word) {
  const box = $('#sym-text');
  if (!box) return;
  box.innerHTML = '<p class="muted">Searching…</p>';
  try {
    const res = await api(`/api/search?word=${encodeURIComponent(word)}&limit=300`);
    box.innerHTML = `<h3>Text matches (${res.hits.length}${res.truncated ? '+' : ''})</h3>` +
      locItems(res.hits.map((h) => ({ ...h, display: h.path })), { group: true });
  } catch (err) {
    box.innerHTML = `<p class="src-error" style="padding:0">${esc(err.message)}</p>`;
  }
}

$('#src-body').addEventListener('click', (e) => {
  if (String(window.getSelection())) return; // selecting text, not navigating
  const codeEl = e.target.closest('.src-line .code');
  if (!codeEl || !viewer.path) return;
  const hit = wordAt(codeEl, e.clientX, e.clientY);
  if (!hit) return;
  const lineEl = codeEl.parentNode;
  document.querySelectorAll('.src-line.sym-line').forEach((el) => el.classList.remove('sym-line'));
  lineEl.classList.add('sym-line');
  showSymbol({ path: viewer.path, line: Number(lineEl.dataset.line), start: hit.start, word: hit.word }, { jump: e.ctrlKey || e.metaKey });
});

$('#sym').addEventListener('click', (e) => {
  if (e.target.closest('[data-sym-close]')) { $('#sym').hidden = true; return; }
  const go = e.target.closest('[data-go], a.loc');
  if (go) openSource(go.dataset.go ?? go.dataset.src, Number(go.dataset.line) || 0);
});

function historyStep(delta) {
  const i = viewer.index + delta;
  if (i < 0 || i >= viewer.history.length) return;
  viewer.index = i;
  const { path, line } = viewer.history[i];
  openSource(path, line, { push: false });
}
$('#src-back').onclick = () => historyStep(-1);
$('#src-fwd').onclick = () => historyStep(1);

function closeSource() {
  $('#modal').hidden = true;
  $('#src-body').innerHTML = '';
  $('#sym').hidden = true;
  viewer.path = null;
}

/** Lines selected in the viewer (1-based, inclusive), or null for the whole file. */
function selectedLines() {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || !$('#src-body').contains(sel.anchorNode)) return null;
  const lineOf = (n) => Number((n.nodeType === Node.TEXT_NODE ? n.parentNode : n).closest?.('.src-line')?.dataset.line);
  const a = lineOf(sel.anchorNode);
  const b = lineOf(sel.focusNode);
  if (!a || !b) return null;
  return [Math.min(a, b), Math.max(a, b)];
}

$('#src-ask').onclick = () => {
  if (!viewer.path || !viewer.lines.length) return;
  const range = selectedLines();
  state.askSelection = { path: viewer.path, start: range?.[0] ?? null, end: range?.[1] ?? null };
  closeSource();
  switchTab('ask');
};

$('#modal').addEventListener('click', (e) => {
  const open = e.target.closest('[data-open]');
  if (open) return openSource(open.dataset.open, 0);
  if (e.target.id === 'modal') closeSource();
});
$('#src-close').onclick = closeSource;

// ---------- ask ----------

state.chat = [];
state.askSelection = null;

function switchTab(tab) {
  state.tab = tab;
  document.querySelectorAll('.tabs [data-tab]').forEach((b) => b.classList.toggle('active', b.dataset.tab === tab));
  $('#panel').hidden = tab !== 'details';
  $('#ask').hidden = tab !== 'ask';
  $('#side').classList.toggle('wide', tab === 'ask');
  if (tab === 'ask') {
    renderAskContext();
    renderChat();
    $('#question').focus();
  }
  if (state.map && $('#screen').hidden) fit();
}
document.querySelectorAll('.tabs [data-tab]').forEach((b) => { b.onclick = () => switchTab(b.dataset.tab); });

/** What the next question is about: current map, selected node, chosen source lines. */
function askContext() {
  const ctx = { scope: state.scope };
  const chips = [];
  const crumbs = state.map?.meta?.crumbs?.join(' › ') || state.repo?.name || 'repository';
  chips.push(`<span class="chip">Map <b>${esc(crumbs)}</b></span>`);
  const node = state.selected?.type === 'node' ? nodeById(state.selected.id) : null;
  if (node) {
    ctx.node = node.id;
    chips.push(`<span class="chip">Component <b>${esc(node.name)}</b></span>`);
  }
  const sel = state.askSelection;
  if (sel) {
    Object.assign(ctx, { path: sel.path, start_line: sel.start, end_line: sel.end });
    const where = sel.start ? `${sel.path}:${sel.start}${sel.end !== sel.start ? `-${sel.end}` : ''}` : sel.path;
    chips.push(`<span class="chip">Code <b>${esc(where)}</b><button type="button" data-drop-sel title="Remove">✕</button></span>`);
  }
  return { ctx, chips, label: [crumbs, node?.name, sel && (sel.start ? `${sel.path}:${sel.start}-${sel.end}` : sel.path)].filter(Boolean).join(' · ') };
}

function renderAskContext() {
  $('#ask-context').innerHTML = askContext().chips.join('');
}

$('#ask-context').addEventListener('click', (e) => {
  if (e.target.closest('[data-drop-sel]')) { state.askSelection = null; renderAskContext(); }
});

function renderEntry(entry) {
  const q = `<div class="msg-q">${esc(entry.question)}</div><div class="msg-ctx">${esc(entry.label)}</div>`;
  if (entry.status === 'running') {
    return q + `<div class="msg-a"><div class="status"><div class="spinner"></div><span>Codex is investigating… ${fmtDuration(entry.elapsed || 0)}</span>
      <button type="button" data-cancel="${entry.job}">Cancel</button></div>${entry.last ? `<div class="last">${esc(entry.last)}</div>` : ''}</div>`;
  }
  if (entry.status !== 'done') {
    return q + `<div class="msg-a"><div class="error">${esc(entry.error || entry.status)}</div>
      <div class="actions"><button type="button" data-retry="${entry.id}">Ask again</button></div></div>`;
  }
  const r = entry.result;
  const cites = r.citations.length ? `<h4 class="md-h">Citations</h4>${evidenceList(r.citations)}` : '';
  return q + `<div class="msg-a">${mdBlock(r.answer)}${cites}
    <p class="meta">${fmtDuration(r.duration_ms)} · ${r.commands} commands · ${fmtTokens(r.input_tokens)} in / ${fmtTokens(r.output_tokens)} out tokens</p></div>`;
}

function renderChat() {
  const chat = $('#chat');
  const stick = chat.scrollTop + chat.clientHeight >= chat.scrollHeight - 40;
  chat.innerHTML = state.chat.length ? state.chat.map(renderEntry).join('') : `<div class="chat-empty">
    <p>Ask anything about this repository. The question includes what you are looking at:</p>
    <ul><li>the current map, and the selected component;</li><li>code lines chosen with <b>Ask about selection</b> in the source viewer.</li></ul>
    <p>Answers cite <code>file:line</code>; click a citation to open it.</p></div>`;
  if (stick) chat.scrollTop = chat.scrollHeight;
  $('#ask-busy').hidden = !state.chat.some((e) => e.status === 'running');
}

async function sendQuestion(question, ctxInfo) {
  const { ctx, label } = ctxInfo || askContext();
  const history = state.chat.filter((e) => e.status === 'done').map((e) => ({ question: e.question, answer: e.result.answer }));
  const entry = { id: state.chat.length, question, label, ctx, status: 'running', elapsed: 0, last: '' };
  state.chat.push(entry);
  renderChat();
  $('#chat').scrollTop = $('#chat').scrollHeight;
  try {
    const res = await post('/api/ask', { question, context: ctx, history });
    entry.job = res.job;
  } catch (err) {
    Object.assign(entry, { status: 'failed', error: err.message });
    return renderChat();
  }
  let since = 0;
  const tick = async () => {
    let job;
    try {
      job = await api(`/api/job/${entry.job}?since=${since}`);
    } catch (err) {
      Object.assign(entry, { status: 'failed', error: err.message });
      return renderChat();
    }
    since = job.next;
    const lines = job.progress.filter((l) => l.startsWith('$ ') || l.startsWith('› '));
    if (lines.length) entry.last = lines[lines.length - 1];
    entry.elapsed = job.elapsed_ms;
    if (job.status === 'running') {
      setTimeout(tick, 1000);
    } else if (job.status === 'done') {
      Object.assign(entry, { status: 'done', result: job.result });
    } else {
      Object.assign(entry, { status: job.status, error: job.status === 'cancelled' ? 'Cancelled.' : job.error });
    }
    if (state.tab === 'ask') renderChat();
    else $('#ask-busy').hidden = !state.chat.some((e) => e.status === 'running');
  };
  tick();
}

function submitQuestion() {
  const box = $('#question');
  const question = box.value.trim();
  if (!question) return;
  box.value = '';
  sendQuestion(question);
}

$('#ask-send').onclick = submitQuestion;
$('#question').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); submitQuestion(); }
});

$('#chat').addEventListener('click', (e) => {
  const loc = e.target.closest('[data-src]');
  if (loc) return openSource(loc.dataset.src, Number(loc.dataset.line) || 0);
  const cancel = e.target.closest('[data-cancel]');
  if (cancel) { cancel.disabled = true; return post(`/api/job/${cancel.dataset.cancel}/cancel`).catch(() => {}); }
  const retry = e.target.closest('[data-retry]');
  if (retry) {
    const old = state.chat[Number(retry.dataset.retry)];
    sendQuestion(old.question, { ctx: old.ctx, label: old.label });
  }
});

window.addEventListener('keydown', (e) => {
  const modalOpen = !$('#modal').hidden;
  if (e.key === 'Escape') {
    if (modalOpen && !$('#sym').hidden) { $('#sym').hidden = true; return; }
    if (modalOpen) return closeSource();
    if (e.target.closest('input, textarea')) return;
    if (state.selected || state.flow !== null) { state.selected = null; state.flow = null; applyHighlight(); renderPanel(); }
    return;
  }
  if (modalOpen && e.altKey && (e.key === 'ArrowLeft' || e.key === 'ArrowRight')) {
    e.preventDefault();
    return historyStep(e.key === 'ArrowLeft' ? -1 : 1);
  }
  if (e.target.closest('input, textarea') || modalOpen) return;
  if (e.key === 'f' || e.key === 'F') fit();
});

// ---------- start ----------

(async () => {
  try {
    state.repo = await api('/api/repo');
    $('#repo').textContent = `${state.repo.root}${state.repo.commit ? ` @ ${state.repo.commit.slice(0, 10)}` : ''}`;
    $('#repo').title = $('#repo').textContent;
  } catch (err) {
    return showError(err.message, '');
  }
  load(scopeFromHash());
})();
