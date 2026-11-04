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

function showMap(map) {
  state.map = map;
  state.layout = layout(map);
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

  $('#nodes').innerHTML = map.nodes.map((node) => {
    const p = L.at[node.id];
    const drill = node.paths.length > 0;
    return `<g class="node ${esc(node.kind)}" data-node="${esc(node.id)}" transform="translate(${p.x},${p.y})" style="--kc:${kindColor(node.kind)}">
      <title>${esc(node.name)}${drill ? ' (double-click to explore inside)' : ''}</title>
      <rect class="box" width="${p.w}" height="${p.h}" rx="9"></rect>
      <rect class="band" width="5" height="${p.h - 16}" x="0" y="8" rx="2"></rect>
      <text class="name" x="16" y="23">${esc(p.name)}</text>
      <text class="kind" x="16" y="41">${esc(node.kind)}</text>
      ${drill ? `<text class="drill" x="${p.w - 12}" y="41" text-anchor="end">${node.explorable ? 'explore ›' : '›'}</text>` : ''}
    </g>`;
  }).join('');
  applyHighlight();
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

let drag = null;
svg.addEventListener('pointerdown', (e) => {
  if (e.button !== 0) return;
  drag = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y, moved: false };
});
window.addEventListener('pointermove', (e) => {
  if (!drag) return;
  const dx = e.clientX - drag.x;
  const dy = e.clientY - drag.y;
  if (!drag.moved && Math.hypot(dx, dy) < 4) return;
  if (!drag.moved) svg.setPointerCapture?.(e.pointerId);
  drag.moved = true;
  svg.classList.add('panning');
  state.view.x = drag.vx + dx;
  state.view.y = drag.vy + dy;
  applyView();
});
window.addEventListener('pointerup', () => {
  svg.classList.remove('panning');
  setTimeout(() => { drag = null; }, 0);
});

svg.addEventListener('click', (e) => {
  if (drag?.moved) return;
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
      ${node.paths.length ? `<div class="actions"><button class="primary" data-act="drill" type="button">Explore inside ›</button></div>` : ''}
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
    <div class="actions"><button data-act="reexplore" type="button">Re-explore</button></div>
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
  else if (t.dataset.act === 'reexplore') return start(state.scope, true);
  applyHighlight();
  renderPanel();
});

// ---------- source viewer (L3) ----------

async function openSource(path, line) {
  $('#modal').hidden = false;
  const body = $('#src-body');
  const parts = path === '.' ? [] : path.split('/');
  $('#src-path').innerHTML = [`<a data-open=".">${esc(state.repo?.name || '.')}</a>`, ...parts.map((p, i) =>
    i === parts.length - 1 ? esc(p) : `<a data-open="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</a>`)].join(' / ') + (line ? `:${line}` : '');
  body.innerHTML = '<p class="muted" style="padding:16px">Loading…</p>';
  let res;
  try {
    res = await api(`/api/source?path=${encodeURIComponent(path)}`);
  } catch (err) {
    body.innerHTML = `<div class="src-error">${esc(err.message)}</div>`;
    return;
  }
  if (res.dir) {
    const base = res.path === '.' ? '' : `${res.path}/`;
    body.innerHTML = '<ul class="dir-list">' + res.entries.map((e) =>
      `<li><a data-open="${esc(base + e.name)}">${esc(e.name)}${e.dir ? '/' : ''}</a></li>`).join('') + '</ul>';
    return;
  }
  const lines = res.text.split('\n');
  if (lines.length > 1 && lines[lines.length - 1] === '') lines.pop();
  body.innerHTML = lines.map((text, i) =>
    `<div class="src-line${i + 1 === line ? ' target' : ''}" id="L${i + 1}"><span class="no">${i + 1}</span><span class="code">${esc(text) || ' '}</span></div>`).join('');
  const target = line && document.getElementById(`L${line}`);
  if (target) target.scrollIntoView({ block: 'center' });
  else body.scrollTop = 0;
}

function closeSource() {
  $('#modal').hidden = true;
  $('#src-body').innerHTML = '';
}

$('#modal').addEventListener('click', (e) => {
  const open = e.target.closest('[data-open]');
  if (open) return openSource(open.dataset.open, 0);
  if (e.target.id === 'modal') closeSource();
});
$('#src-close').onclick = closeSource;

window.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') {
    if (!$('#modal').hidden) return closeSource();
    if (state.selected || state.flow !== null) { state.selected = null; state.flow = null; applyHighlight(); renderPanel(); }
    return;
  }
  if (e.target.closest('input, textarea') || !$('#modal').hidden) return;
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
