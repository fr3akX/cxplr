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
  hidden: new Set(), // ids of components hidden on the current map
};

// ---------- utilities ----------

function esc(text) {
  return String(text ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

/** The agent that makes maps (`index`) or answers questions, examples and walkthroughs (`ask`). */
function engineName(role = 'index') {
  const id = role === 'ask' ? state.repo?.ask_engine || state.repo?.engine : state.repo?.engine;
  return { codex: 'Codex', claude: 'Claude Code', anthropic: 'Claude (native)', openai: 'OpenAI-compatible model' }[id] || 'Codex';
}

function sessionHint(meta) {
  if (meta.engine === 'anthropic' || meta.engine === 'openai') return 'cxplr session; follow-ups and refresh resume it through cxplr';
  return `Continue it in a terminal: ${meta.engine === 'claude' ? 'claude --resume' : 'codex resume'} ${meta.session}`;
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

/** A running job's live model stats: context use against the window, tokens, requests. */
function usageHtml(u) {
  if (!u?.requests) return '';
  const pct = u.context_window ? Math.min(100, Math.round((u.context_tokens * 100) / u.context_window)) : null;
  const ctx = pct === null
    ? `context ${fmtTokens(u.context_tokens)}`
    : `context ${fmtTokens(u.context_tokens)} / ${fmtTokens(u.context_window)} (${pct}%)`;
  const cached = u.input_tokens ? ` (${Math.round((u.cached_input_tokens * 100) / u.input_tokens)}% cached)` : '';
  const meter = pct === null ? '' : `<span class="ctx-meter" title="Context window used"><span style="width:${pct}%"></span></span>`;
  return `<div class="usage">${meter}<span>${ctx} · ${fmtTokens(u.input_tokens)} in${cached} / ${fmtTokens(u.output_tokens)} out · ${u.requests} request${u.requests === 1 ? '' : 's'}</span></div>`;
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
  stopSim();
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
  if (res.status === 'ready') {
    state.freshness = res.freshness;
    state.children = res.children || {};
    return showMap(res.map);
  }
  if (res.status === 'running') return watch(res.job, scope);
  showMissing(scope);
}

/** Never explored: browsing is cache-only, so exploring is an explicit choice. */
function showMissing(scope) {
  const parent = scope.includes('/') ? scope.slice(0, scope.lastIndexOf('/')) : '';
  showScreen(`
    <h2>${scope ? 'This component has no map yet' : 'This project has no map yet'}</h2>
    <p class="muted">Exploring runs ${esc(engineName())} read-only, typically a few minutes.
      To map the whole project up front, run <code>cxplr ${esc(state.repo?.root || '.')} --index</code>.</p>
    <div class="row">
      <button class="primary" id="explore-now" type="button">Explore now</button>
      ${scope ? '<button id="up" type="button">Back up one level</button>' : ''}
    </div>`);
  renderPanelNote('');
  $('#explore-now').onclick = () => start(scope, false);
  if (scope) $('#up').onclick = () => go(parent);
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
    <div class="muted">${esc(engineName())} is reading the repository (read-only). <span id="elapsed"></span></div>
    <div id="job-usage"></div>
    <pre class="log" id="log"></pre>
    <div class="row"><button id="cancel" type="button">Cancel</button></div>`);
  renderPanelNote(`Exploration in progress. The map appears here when ${engineName()} finishes and its citations are verified.`);
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
    const usage = $('#job-usage');
    if (usage) usage.innerHTML = usageHtml(job.usage);
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
  return `cxplr:layout:${state.repo?.root}@${state.repo?.commit || 'no-commit'}:${state.scope}`;
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
  const moved = Object.fromEntries(Object.entries(at).map(([id, p]) => [id, [Math.round(p.cx), Math.round(p.cy)]]));
  try {
    // Keep the saved positions of hidden components for when they are shown again.
    localStorage.setItem(layoutKey(), JSON.stringify({ ...savedPositions(), ...moved }));
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

/** Components hidden on this map, kept per map in this browser (across commits). */
function hiddenKey() {
  return `cxplr:hidden:${state.repo?.root}:${state.scope}`;
}

function loadHidden(map) {
  let ids = [];
  try {
    ids = JSON.parse(localStorage.getItem(hiddenKey())) || [];
  } catch {
    // Unreadable or storage disabled: show everything.
  }
  const known = new Set(map.nodes.map((n) => n.id));
  return new Set(ids.filter((id) => known.has(id)));
}

function isHidden(id) {
  return state.hidden.has(id);
}

/** The map as drawn: hidden components and the arrows touching them left out. */
function visibleMap(map) {
  if (!state.hidden.size) return map;
  return {
    ...map,
    nodes: map.nodes.filter((n) => !isHidden(n.id)),
    edges: map.edges.filter((e) => !isHidden(e.from) && !isHidden(e.to)),
  };
}

/** Hide or show components, then lay out what remains. */
function setHidden(ids, hide) {
  for (const id of ids) {
    if (hide) state.hidden.add(id);
    else state.hidden.delete(id);
  }
  try {
    if (state.hidden.size) localStorage.setItem(hiddenKey(), JSON.stringify([...state.hidden]));
    else localStorage.removeItem(hiddenKey());
  } catch {
    // Storage disabled: applies to this view only.
  }
  // A hidden box (or an arrow touching one) cannot stay selected.
  const sel = state.selected;
  const edge = sel?.type === 'edge' ? state.map.edges[sel.index] : null;
  const gone = sel?.type === 'node' ? isHidden(sel.id) : Boolean(edge) && (isHidden(edge.from) || isHidden(edge.to));
  if (hide && gone) state.selected = null;
  stopSim();
  state.layout = computeLayout(state.map);
  renderGraph();
  fit();
  renderPanel();
}

function computeLayout(map) {
  const L = layout(visibleMap(map));
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
  state.hidden = loadHidden(map);
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
  if (state.pendingSim) playPendingSim();
  if (walker) walkMapChanged();
}

function renderGraph() {
  renderNodes();
  renderEdges();
}

/** Edge geometry follows node positions; called again while a node is dragged. */
function renderEdges() {
  const { map, layout: L } = state;
  const shown = (e) => !isHidden(e.from) && !isHidden(e.to);
  const pairCount = new Map();
  const pairSeen = new Map();
  const pairKey = (e) => [e.from, e.to].sort().join('\u0000');
  map.edges.forEach((e) => { if (shown(e)) pairCount.set(pairKey(e), (pairCount.get(pairKey(e)) || 0) + 1); });

  state.edgeGeom = map.edges.map((e) => {
    if (!shown(e)) return null;
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
    // Arrows of hidden components keep an empty placeholder, so elements stay in edge-index order.
    if (!g) return `<g class="edge hidden-edge" data-edge="${i}"><path class="line"></path></g>`;
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
  $('#nodes').innerHTML = map.nodes.filter((node) => !isHidden(node.id)).map((node) => {
    const p = L.at[node.id];
    const drill = node.paths.length > 0;
    return `<g class="node ${esc(node.kind)}" data-node="${esc(node.id)}" transform="${nodeTransform(p)}" style="--kc:${kindColor(node.kind)}">
      <title>${esc(node.name)} (drag to move${drill ? ', double-click to explore inside' : ''})</title>
      <rect class="box" width="${p.w}" height="${p.h}" rx="9"></rect>
      <rect class="band" width="5" height="${p.h - 16}" x="0" y="8" rx="2"></rect>
      <text class="name" x="16" y="23">${esc(p.name)}</text>
      <text class="kind" x="16" y="41">${esc(node.kind)}</text>
      ${drill ? `<text class="drill ${childState(node.id)}" x="${p.w - 12}" y="41" text-anchor="end">${drillLabel(node)}</text>` : ''}
    </g>`;
  }).join('');
  if (walker) markWalkNode();
}

/** `fresh` / `stale` when the node's inner map is cached, `none` otherwise. */
function childState(id) {
  return state.children?.[id] || 'none';
}

function drillLabel(node) {
  const s = childState(node.id);
  if (s === 'fresh') return 'open ›';
  if (s === 'stale') return 'open (stale) ›';
  return node.explorable ? 'explore ›' : '›';
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
  if (walker) return applyWalkHighlight(nodes, edges, hiEdge, overlay);

  if (flow !== null && map.flows[flow]) {
    const steps = map.flows[flow].steps;
    const litNodes = new Set(steps.flatMap((s) => [s.from, s.to]));
    const badges = new Map(); // edge index or ghost key -> { mid, numbers }
    steps.forEach((s, n) => {
      if (isHidden(s.from) || isHidden(s.to)) return; // numbering stays that of the flow
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
    drawStepBadges(overlay, badges);
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

/** Step-number badges at the middle of arrows: `badges` maps a key to { mid, numbers }. */
function drawStepBadges(overlay, badges) {
  for (const { mid, numbers } of badges.values()) {
    const text = numbers.join(',');
    const r = Math.max(10, text.length * 3.6 + 5);
    overlay.insertAdjacentHTML('beforeend', `<g class="step-badge" transform="translate(${mid[0]},${mid[1] + 9})">
      <rect x="${-r}" y="-10" width="${2 * r}" height="20" rx="10" style="fill:var(--accent)"></rect>
      <text text-anchor="middle" y="4">${esc(text)}</text></g>`);
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
  // While a walkthrough docks the viewer over the map, fit into the part left uncovered.
  const dock = walker && !$('#modal').hidden ? $('.modal-box').getBoundingClientRect().left : r.right;
  const width = Math.max(160, Math.min(r.right, dock) - r.left);
  const pad = 48;
  const k = Math.min((width - 2 * pad) / box.width, (r.height - 2 * pad) / box.height, 1.25);
  state.view = { k, x: (width - box.width * k) / 2 - box.x * k, y: (r.height - box.height * k) / 2 - box.y * k };
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
  if (state.flow !== null && !state.selected) fillTraceBox();
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
        <button data-act="${isHidden(node.id) ? 'show' : 'hide'}" type="button" title="${isHidden(node.id) ? 'Show it on the map again' : 'Leave it out of the map, with its arrows (H)'}">${isHidden(node.id) ? 'Show on map' : 'Hide from map'}</button>
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
      ${meta ? `${new Date(meta.generated_at * 1000).toLocaleString()} · ${meta.refreshed ? 'refreshed in ' : ''}${fmtDuration(meta.duration_ms)} · ${meta.commands} commands · ${fmtTokens(meta.input_tokens)} in${meta.cached_input_tokens ? ` (${fmtTokens(meta.cached_input_tokens)} cached)` : ''} / ${fmtTokens(meta.output_tokens)} out tokens` : ''}
      ${meta?.model ? ` · ${esc(meta.model)}` : ''}${meta?.effort ? ` · effort ${esc(meta.effort)}` : ''}
      ${meta?.session ? ` · session <code title="${esc(sessionHint(meta))}">${esc(meta.session.slice(0, 8))}</code>` : ''}
    </p>
    <p class="meta">${map.nodes.length} nodes${state.hidden.size ? ` (${state.hidden.size} hidden · <a class="path-link" data-act="show-all">show all</a>)` : ''} · ${map.edges.length} interactions ·
      <span class="${verified === allEvidence.length ? '' : 'warn'}">${verified}/${allEvidence.length} citations verified</span></p>
    ${staleBanner()}
    <div class="actions"><button data-act="reexplore" type="button">${state.freshness?.state === 'stale' ? 'Refresh' : 'Re-explore'}</button>
      ${state.customLayout ? '<button data-act="reset-layout" type="button" title="Discard dragged positions">Reset layout</button>' : ''}</div>
    <h3>Data flows</h3>
    ${map.flows.length ? `<div class="sim-controls">
        <label>Inputs <input id="sim-n" type="number" min="1" max="20" value="${sim.n}"></label>
        <label>Speed <select id="sim-speed">${[['0.5', 'slow'], ['1', 'normal'], ['2', 'fast']].map(([v, l]) => `<option value="${v}" ${String(sim.speed) === v ? 'selected' : ''}>${l}</option>`).join('')}</select></label>
        ${sim.running ? '<button type="button" data-sim-stop>■ Stop</button>' : ''}
      </div>` : ''}
    ${map.flows.length ? '<ul class="list">' + map.flows.map((f, i) => `<li class="click flow-item ${i === state.flow ? 'active' : ''}" data-flow="${i}">
        <button type="button" class="play" data-sim="${i}" title="Simulate ${sim.n} inputs travelling this flow">▶</button>
        <b>${esc(f.name)}</b><div class="muted">${esc(f.description)}</div></li>`).join('') + '</ul>' : '<p class="muted">None identified.</p>'}
    ${flow ? `<h3>Steps: ${esc(flow.name)}</h3>${simStatus()}<ul class="list" id="flow-steps">` + flow.steps.map((s, n) => {
      let i = map.edges.findIndex((e) => e.from === s.from && e.to === s.to);
      if (i < 0) i = map.edges.findIndex((e) => e.from === s.to && e.to === s.from);
      const inner = childState(s.to) !== 'none' && nodeById(s.to)?.paths.length;
      return `<li class="${i >= 0 ? 'click' : ''}" data-step="${n}" ${i >= 0 ? `data-sel-edge="${i}"` : ''}><div class="step"><span class="step-no">${n + 1}</span>
        <span><b>${esc(nodeById(s.from)?.name)} → ${esc(nodeById(s.to)?.name)}</b><div class="muted">${esc(s.action)}</div>
        ${inner ? `<a class="path-link" data-follow="${esc(s.to)}" data-from="${esc(s.from)}">Follow inside ${esc(nodeById(s.to).name)} ›</a>` : ''}</span></div></li>`;
    }).join('') + '</ul><h3>Example inputs</h3><div id="trace-box"></div>' : ''}
    <h3>Components</h3>
    <ul class="list">${map.nodes.filter((n) => !isHidden(n.id)).map(componentItem).join('')}</ul>
    ${state.hidden.size ? `<h3>Hidden (${state.hidden.size}) · <a class="path-link" data-act="show-all">show all</a></h3>
      <ul class="list">${map.nodes.filter((n) => isHidden(n.id)).map(componentItem).join('')}</ul>` : ''}
    ${map.diagnostics.length ? `<h3>Diagnostics</h3><details><summary>${map.diagnostics.length} issues in the agent's answer</summary>
      <ul>${map.diagnostics.map((d) => `<li class="meta">${esc(d)}</li>`).join('')}</ul></details>` : ''}
    <h3>How to read</h3>
    <p class="meta">Click a box or arrow for details and evidence. Double-click a box, or use “Explore inside”, to zoom in one level.
      Drag a box to move it; positions are kept per map in this browser. Hide a box you do not need with H (or “hide” in the list below);
      hidden boxes and their arrows leave the map until shown again.
      Arrows point the way requests or data travel. Dashed boxes are external systems or actors.</p>
    <div class="legend">
      ${legendLine('call / http / rpc', '')}${legendLine('event / queue', '7 4')}
      ${legendLine('read / write / file', '2 3')}${legendLine('deploy / config / import', '12 4 2 4')}
    </div>`;
}

function staleBanner() {
  const f = state.freshness;
  if (f?.state !== 'stale') return '';
  const since = f.since ? ` since commit <code>${esc(f.since.slice(0, 10))}</code>` : '';
  const files = f.changed.length
    ? `<details><summary>${f.total} file${f.total === 1 ? '' : 's'} changed${since}</summary><ul class="changed">${f.changed.map((p) => `<li><a class="path-link" data-src="${esc(p)}">${esc(p)}</a></li>`).join('')}${f.total > f.changed.length ? `<li class="muted">… ${f.total - f.changed.length} more</li>` : ''}</ul></details>`
    : `<p class="meta">The covered content changed${since}.</p>`;
  return `<div class="stale"><b>Stale map.</b> It describes an earlier version of this code; Refresh re-explores it.${files}</div>`;
}

function legendLine(text, dash) {
  return `<span><svg width="34" height="8"><line x1="0" y1="4" x2="34" y2="4" stroke="var(--edge)" stroke-width="1.6" stroke-dasharray="${dash}"></line></svg>${esc(text)}</span>`;
}

function componentItem(n) {
  const hidden = isHidden(n.id);
  return `<li class="click${hidden ? ' is-hidden' : ''}" data-sel-node="${esc(n.id)}"><span class="swatch" style="--kc:${kindColor(n.kind)}"></span><b>${esc(n.name)}</b>
    <span class="muted"> ${esc(n.kind)}</span>${n.paths.length ? ` <a class="path-link drill-${childState(n.id)}" data-drill="${esc(n.id)}">${esc(drillLabel(n))}</a>` : ''}
    <button type="button" class="vis" ${hidden ? 'data-show' : 'data-hide'}="${esc(n.id)}" title="${hidden ? 'Show it on the map' : 'Hide it from the map'}">${hidden ? 'show' : 'hide'}</button></li>`;
}

$('#panel').addEventListener('click', (e) => {
  const t = e.target.closest('[data-hide], [data-show], [data-sim], [data-sim-stop], [data-follow], [data-src], [data-drill], [data-sel-node], [data-sel-edge], [data-flow], [data-act]');
  if (!t) return;
  if (t.dataset.hide !== undefined) return setHidden([t.dataset.hide], true);
  if (t.dataset.show !== undefined) return setHidden([t.dataset.show], false);
  if (t.dataset.sim !== undefined) return startFlowSim(Number(t.dataset.sim));
  if (t.dataset.simStop !== undefined) return stopSim(true);
  if (t.dataset.follow !== undefined) return followInside(t.dataset.follow, t.dataset.from);
  if (t.dataset.src !== undefined) return openSource(t.dataset.src, Number(t.dataset.line) || 0);
  if (t.dataset.drill !== undefined) return go(childScope(t.dataset.drill));
  if (t.dataset.selNode !== undefined) { state.flow = null; state.selected = { type: 'node', id: t.dataset.selNode }; }
  else if (t.dataset.selEdge !== undefined) { state.flow = null; state.selected = { type: 'edge', index: Number(t.dataset.selEdge) }; }
  else if (t.dataset.flow !== undefined) { const i = Number(t.dataset.flow); state.selected = null; state.flow = state.flow === i ? null : i; }
  else if (t.dataset.act === 'back') state.selected = null;
  else if (t.dataset.act === 'drill') return go(childScope(state.selected.id));
  else if (t.dataset.act === 'ask') return switchTab('ask');
  else if (t.dataset.act === 'hide') return setHidden([state.selected.id], true);
  else if (t.dataset.act === 'show') return setHidden([state.selected.id], false);
  else if (t.dataset.act === 'show-all') return setHidden([...state.hidden], false);
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

// ---------- simulation ----------

const SIM_PX_PER_MS = 0.32; // token speed at "normal"
const SIM_MIN_STEP_MS = 450;
const SIM_PAUSE_MS = 260; // dwell at each box between steps

/** Running animation of tokens along step paths; each token carries its own steps. */
const sim = { n: 5, speed: 1, running: false, raf: 0, tokens: [], label: '', done: 0, followable: [] };

$('#panel').addEventListener('change', (e) => {
  if (e.target.id === 'sim-n') sim.n = Math.max(1, Math.min(20, Number(e.target.value) || 1));
  if (e.target.id === 'sim-speed') sim.speed = Number(e.target.value) || 1;
});

function simStatus() {
  if (!sim.running && !sim.tokens.length) return '';
  return `<p class="meta" id="sim-status">${esc(simStatusText())}</p>`;
}

function simStatusText() {
  const total = sim.tokens.length;
  return sim.running ? `Simulating ${total} input${total === 1 ? '' : 's'}: ${sim.done}/${total} reached the end` : `${sim.done}/${total} inputs reached the end`;
}

/** Geometry of one hop: the drawn edge (either direction) or a guide curve. */
function hopGeometry(step) {
  const map = state.map;
  let i = map.edges.findIndex((e) => e.from === step.from && e.to === step.to);
  let reverse = false;
  if (i < 0) {
    i = map.edges.findIndex((e) => e.from === step.to && e.to === step.from);
    reverse = i >= 0;
  }
  const d = i >= 0 ? state.edgeGeom[i].d : edgePath(state.layout, step, 0).d;
  return { d, reverse };
}

function stopSim(keepStatus = false) {
  cancelAnimationFrame(sim.raf);
  sim.running = false;
  $('#sim').innerHTML = '';
  document.querySelectorAll('#nodes .node.sim-hit').forEach((el) => el.classList.remove('sim-hit'));
  if (!keepStatus) sim.tokens = [];
  const status = $('#sim-status');
  if (status) status.textContent = simStatusText();
}

/**
 * Animate tokens. `routes` holds one step list per token; tokens start staggered
 * and move at constant speed, pausing at each box.
 */
function runSim(routes, { colors = [], labels = [], onStep } = {}) {
  stopSim();
  const layer = $('#sim');
  const measure = (d) => {
    const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    p.setAttribute('d', d);
    p.setAttribute('class', 'sim-track');
    layer.appendChild(p);
    return p;
  };
  const stagger = 650 / sim.speed;
  sim.tokens = routes.map((steps, k) => {
    const hops = steps.filter((s) => state.layout.at[s.from] && state.layout.at[s.to] && s.from !== s.to).map((s) => {
      const { d, reverse } = hopGeometry(s);
      const path = measure(d);
      const len = path.getTotalLength();
      return { step: s, path, len, reverse, ms: Math.max(SIM_MIN_STEP_MS, len / SIM_PX_PER_MS) / sim.speed };
    });
    const g = document.createElementNS('http://www.w3.org/2000/svg', 'g');
    g.setAttribute('class', 'sim-token');
    if (colors[k]) g.style.setProperty('--tc', colors[k]);
    g.innerHTML = `<circle r="7"></circle><text text-anchor="middle" y="3.5">${esc(labels[k] ?? k + 1)}</text>`;
    layer.appendChild(g);
    return { hops, g, start: k * stagger, hop: -1, finished: false };
  });
  sim.done = 0;
  sim.running = true;
  const t0 = performance.now();
  const pulse = (id) => {
    const el = document.querySelector(`#nodes [data-node="${CSS.escape(id)}"]`);
    if (!el) return;
    el.classList.remove('sim-hit');
    void el.getBoundingClientRect(); // restart the CSS animation
    el.classList.add('sim-hit');
  };
  const frame = (now) => {
    const t = now - t0;
    let active = 0;
    for (const [k, tok] of sim.tokens.entries()) {
      if (tok.finished) continue;
      let local = t - tok.start;
      if (local < 0 || !tok.hops.length) {
        tok.g.style.opacity = '0';
        if (!tok.hops.length && local >= 0) { tok.finished = true; sim.done++; }
        active++;
        continue;
      }
      tok.g.style.opacity = '1';
      let h = 0;
      while (h < tok.hops.length && local > tok.hops[h].ms + SIM_PAUSE_MS) {
        local -= tok.hops[h].ms + SIM_PAUSE_MS;
        h++;
      }
      if (h >= tok.hops.length) {
        tok.finished = true;
        sim.done++;
        tok.g.style.opacity = '0';
        pulse(tok.hops[tok.hops.length - 1].step.to);
        continue;
      }
      active++;
      const hop = tok.hops[h];
      if (h !== tok.hop) {
        tok.hop = h;
        pulse(hop.step.from);
        onStep?.(k, h, hop.step);
      }
      const p = Math.min(1, local / hop.ms);
      const pt = hop.path.getPointAtLength((hop.reverse ? 1 - p : p) * hop.len);
      // Counter-scale so tokens stay readable at any zoom.
      tok.g.setAttribute('transform', `translate(${pt.x},${pt.y}) scale(${1 / Math.max(state.view.k, 0.05)})`);
    }
    const status = $('#sim-status');
    if (status) status.textContent = simStatusText();
    if (active) sim.raf = requestAnimationFrame(frame);
    else {
      sim.running = false;
      if (status) status.textContent = simStatusText();
      if (!state.selected) renderPanel();
    }
  };
  sim.raf = requestAnimationFrame(frame);
}

/** Simulate N identical inputs travelling one flow of the current map. */
function startFlowSim(index, { n = sim.n } = {}) {
  const flow = state.map.flows[index];
  if (!flow) return;
  state.selected = null;
  state.flow = index;
  applyHighlight();
  sim.tokens = [{}]; // makes the status line render before the run starts
  renderPanel();
  runSim(Array.from({ length: n }, () => flow.steps), {
    onStep: (k, h) => {
      if (k !== 0) return; // the lead token marks the current step
      document.querySelectorAll('#flow-steps li').forEach((li) => li.classList.toggle('sim-active', Number(li.dataset.step) === h));
    },
  });
}

/** Lowercase word set for loose name matching across levels. */
function words(text) {
  return new Set(String(text).toLowerCase().split(/[^a-z0-9]+/).filter((w) => w.length > 2));
}

/**
 * Continue a simulation one level down: open the component's map and play the flow
 * whose steps involve the neighbour the input came from (by name).
 */
function followInside(nodeId, fromId) {
  const from = nodeById(fromId)?.name || '';
  state.pendingSim = { from, n: sim.n };
  go(childScope(nodeId));
}

function playPendingSim() {
  const pending = state.pendingSim;
  state.pendingSim = null;
  if (!pending || !state.map?.flows.length) return;
  const want = words(pending.from);
  let best = 0;
  let bestScore = -1;
  state.map.flows.forEach((f, i) => {
    const names = f.steps.flatMap((s) => [nodeById(s.from)?.name, nodeById(s.to)?.name, s.action]).join(' ');
    const have = words(`${f.name} ${f.description} ${names}`);
    const score = [...want].filter((w) => have.has(w)).length;
    if (score > bestScore) { best = i; bestScore = score; }
  });
  startFlowSim(best, { n: pending.n });
}

// ---------- example inputs (traces) ----------

const SCENARIO_COLORS = ['#2f6fe4', '#e8590c', '#2b8a3e', '#ae3ec9', '#c92a2a', '#1098ad', '#f08c00', '#5c7cfa'];
const traceState = new Map(); // `${scope}#${flow}` -> { status, traces, job, last, error }

function traceId(flow = state.flow) {
  return `${state.scope}#${flow}`;
}

async function fillTraceBox() {
  const box = $('#trace-box');
  if (!box) return;
  const id = traceId();
  let t = traceState.get(id);
  if (!t) {
    t = { status: 'loading' };
    traceState.set(id, t);
    try {
      const res = await api(`/api/trace?scope=${encodeURIComponent(state.scope)}&flow=${state.flow}`);
      Object.assign(t, res.status === 'ready' ? { status: 'ready', traces: res.traces } : { status: res.status, job: res.job });
      if (res.status === 'running') pollTrace(id, t);
    } catch (err) {
      Object.assign(t, { status: 'failed', error: err.message });
    }
  }
  if (traceId() === id) renderTraceBox(t);
}

function renderTraceBox(t) {
  const box = $('#trace-box');
  if (!box) return;
  if (t.status === 'loading') { box.innerHTML = '<p class="muted">Loading…</p>'; return; }
  if (t.status === 'missing' || t.status === 'failed' || t.status === 'cancelled') {
    box.innerHTML = `${t.error ? `<p class="error-text">${esc(t.error)}</p>` : ''}
      <p class="meta">Concrete inputs that take different branches through this flow, with the data at every hop and the code that decides it.</p>
      <div class="actions"><button type="button" data-trace-gen>Generate example inputs</button></div>
      <p class="meta">Runs ${esc(engineName('ask'))} read-only (about 1–3 minutes); the result is cached with the map.</p>`;
    return;
  }
  if (t.status === 'running') {
    box.innerHTML = `<div class="msg-a"><div class="status"><div class="spinner"></div><span>${esc(engineName('ask'))} is tracing inputs… ${fmtDuration(t.elapsed || 0)}</span>
      <button type="button" data-trace-cancel>Cancel</button></div>${usageHtml(t.usage)}${t.last ? `<div class="last">${esc(t.last)}</div>` : ''}</div>`;
    return;
  }
  const { scenarios, meta, diagnostics } = t.traces;
  if (!scenarios.length) {
    box.innerHTML = `<p class="muted">${esc(engineName('ask'))} returned no scenario on this map.</p><div class="actions"><button type="button" data-trace-gen>Try again</button></div>`;
    return;
  }
  box.innerHTML = `<div class="actions" style="margin-top:0"><button type="button" class="primary" data-trace-play>▶ Play ${scenarios.length} examples</button>
      <button type="button" data-trace-gen title="Generate new examples">Regenerate</button></div>
    <ul class="list scenarios">${scenarios.map((sc, k) => `<li class="scenario" data-scenario="${k}" style="--tc:${SCENARIO_COLORS[k % SCENARIO_COLORS.length]}">
      <div class="sc-head"><span class="sc-dot">${String.fromCharCode(65 + k)}</span><b>${esc(sc.name)}</b>
        <button type="button" class="play" data-trace-play="${k}" title="Play this example">▶</button></div>
      <div class="sc-io"><span class="muted">in</span> <code>${esc(trunc(sc.input, 160))}</code></div>
      <div class="sc-io"><span class="muted">out</span> ${esc(sc.outcome)}</div>
      <div class="sc-now" id="sc-now-${k}"></div>
      <div class="sc-walk" id="sc-walk-${k}"></div>
      <details><summary>${sc.steps.length} hops</summary><ol class="sc-steps">${sc.steps.map((s) => `<li>
        <b>${esc(nodeById(s.from)?.name || s.from)} → ${esc(nodeById(s.to)?.name || s.to)}</b><div class="muted">${esc(s.action)}</div>
        ${s.payload ? `<pre class="payload">${esc(s.payload)}</pre>` : ''}${s.evidence.length ? evidenceList(s.evidence) : ''}</li>`).join('')}</ol></details>
    </li>`).join('')}</ul>
    ${diagnostics?.length ? `<details><summary class="meta">${diagnostics.length} hops not on the map were dropped</summary><ul>${diagnostics.map((d) => `<li class="meta">${esc(d)}</li>`).join('')}</ul></details>` : ''}
    ${meta ? `<p class="meta">${fmtDuration(meta.duration_ms)} · ${meta.commands} commands · ${fmtTokens(meta.input_tokens)} in / ${fmtTokens(meta.output_tokens)} out tokens</p>` : ''}`;
  scenarios.forEach((_, k) => renderWalkStatus(k));
}

function pollTrace(id, t) {
  let since = 0;
  const tick = async () => {
    let job;
    try {
      job = await api(`/api/job/${t.job}?since=${since}`);
    } catch (err) {
      Object.assign(t, { status: 'failed', error: err.message });
      if (traceId() === id) renderTraceBox(t);
      return;
    }
    since = job.next;
    const lines = job.progress.filter((l) => l.startsWith('$ ') || l.startsWith('› '));
    if (lines.length) t.last = lines[lines.length - 1];
    t.elapsed = job.elapsed_ms;
    t.usage = job.usage;
    if (job.status === 'running') setTimeout(tick, 1000);
    else if (job.status === 'done') Object.assign(t, { status: 'ready', traces: job.result });
    else Object.assign(t, { status: job.status, error: job.status === 'cancelled' ? '' : job.error });
    if (traceId() === id) renderTraceBox(t);
  };
  tick();
}

async function generateTrace() {
  const id = traceId();
  const t = { status: 'running', elapsed: 0 };
  traceState.set(id, t);
  renderTraceBox(t);
  try {
    const res = await post('/api/trace', { scope: state.scope, flow: state.flow });
    t.job = res.job;
    pollTrace(id, t);
  } catch (err) {
    Object.assign(t, { status: 'failed', error: err.message });
    renderTraceBox(t);
  }
}

/** Play scenarios: one token per scenario on its own route, showing its data at each hop. */
function playScenarios(only) {
  const t = traceState.get(traceId());
  if (t?.status !== 'ready') return;
  const all = t.traces.scenarios.map((sc, k) => ({ sc, k }));
  const picked = only === undefined ? all : all.filter(({ k }) => k === only);
  document.querySelectorAll('.sc-now').forEach((el) => { el.textContent = ''; });
  runSim(picked.map(({ sc }) => sc.steps), {
    colors: picked.map(({ k }) => SCENARIO_COLORS[k % SCENARIO_COLORS.length]),
    labels: picked.map(({ k }) => String.fromCharCode(65 + k)),
    onStep: (i, h, step) => {
      const el = $(`#sc-now-${picked[i].k}`);
      if (el) el.innerHTML = `<span class="muted">now</span> ${esc(nodeById(step.from)?.name || step.from)} → ${esc(nodeById(step.to)?.name || step.to)}${step.payload ? `<pre class="payload">${esc(step.payload)}</pre>` : ''}`;
    },
  });
}

$('#panel').addEventListener('click', (e) => {
  if (e.target.closest('[data-trace-gen]')) return generateTrace();
  const cancel = e.target.closest('[data-trace-cancel]');
  if (cancel) {
    const t = traceState.get(traceId());
    cancel.disabled = true;
    if (t?.job) post(`/api/job/${t.job}/cancel`).catch(() => {});
    return;
  }
  const play = e.target.closest('[data-trace-play]');
  if (play) {
    e.stopPropagation();
    return playScenarios(play.dataset.tracePlay === '' ? undefined : Number(play.dataset.tracePlay));
  }
  const walk = e.target.closest('[data-walk]');
  if (walk) return stepThroughCode(Number(walk.dataset.walk));
  const walkCancel = e.target.closest('[data-walk-cancel]');
  if (walkCancel) {
    const w = walkState.get(walkId(Number(walkCancel.dataset.walkCancel)));
    walkCancel.disabled = true;
    if (w?.job) post(`/api/job/${w.job}/cancel`).catch(() => {});
  }
});

// ---------- code walkthrough ----------

/**
 * One example input followed through the code. The viewer docks to the right so the
 * map stays visible; each step opens its file, highlights its lines with a note under
 * them, and lights up the map component whose paths contain the file.
 */
const walkState = new Map(); // `${scope}#${flow}#${scenario}` -> { status, walk, job, elapsed, last, error }
let walker = null; // { walk, index, node }

function walkId(k) {
  return `${traceId()}#${k}`;
}

function renderWalkStatus(k) {
  const el = $(`#sc-walk-${k}`);
  if (!el) return;
  const w = walkState.get(walkId(k));
  if (w?.status === 'running') {
    el.innerHTML = `<div class="status"><div class="spinner"></div><span>${esc(engineName('ask'))} is walking the code… ${fmtDuration(w.elapsed || 0)}</span>
      <button type="button" data-walk-cancel="${k}">Cancel</button></div>${usageHtml(w.usage)}${w.last ? `<div class="last">${esc(w.last)}</div>` : ''}`;
    return;
  }
  const ready = w?.status === 'ready';
  const steps = ready ? w.walk.steps.length : 0;
  el.innerHTML = `<button type="button" data-walk="${k}" title="${ready ? 'Step through the code this input runs' : `Follow this input through the code, step by step (${esc(engineName('ask'))}, read-only, about 1–3 minutes; cached with the map)`}">
      ${ready ? `Step through code · ${steps} steps` : 'Walk through code'}</button>
    ${w?.status === 'failed' ? `<span class="error-text">${esc(w.error)}</span>` : ''}`;
}

async function stepThroughCode(k) {
  const id = walkId(k);
  let w = walkState.get(id);
  if (w?.status === 'running') return;
  if (w?.status === 'ready') return openWalk(w.walk);
  w = { status: 'running', elapsed: 0 };
  walkState.set(id, w);
  renderWalkStatus(k);
  try {
    const query = `scope=${encodeURIComponent(state.scope)}&flow=${state.flow}&scenario=${k}`;
    const res = await api(`/api/walk?${query}`);
    if (res.status === 'ready') {
      Object.assign(w, { status: 'ready', walk: res.walk });
      renderWalkStatus(k);
      return openWalk(res.walk);
    }
    w.job = res.status === 'running' ? res.job : (await post('/api/walk', { scope: state.scope, flow: state.flow, scenario: k })).job;
    pollWalk(id, k, w);
  } catch (err) {
    Object.assign(w, { status: 'failed', error: err.message });
    renderWalkStatus(k);
  }
}

function pollWalk(id, k, w) {
  let since = 0;
  const tick = async () => {
    let job;
    try {
      job = await api(`/api/job/${w.job}?since=${since}`);
    } catch (err) {
      Object.assign(w, { status: 'failed', error: err.message });
      if (walkId(k) === id) renderWalkStatus(k);
      return;
    }
    since = job.next;
    const lines = job.progress.filter((l) => l.startsWith('$ ') || l.startsWith('› '));
    if (lines.length) w.last = lines[lines.length - 1];
    w.elapsed = job.elapsed_ms;
    w.usage = job.usage;
    if (job.status === 'running') setTimeout(tick, 1000);
    else if (job.status === 'done') Object.assign(w, { status: 'ready', walk: job.result });
    else Object.assign(w, { status: job.status === 'cancelled' ? 'missing' : 'failed', error: job.error });
    if (walkId(k) !== id) return;
    renderWalkStatus(k);
    if (w.status === 'ready') openWalk(w.walk);
  };
  tick();
}

function openWalk(walk) {
  if (!walk.steps.length) return;
  if (walker) exitWalk();
  walker = {
    walk,
    index: 0,
    node: null,
    scope: state.scope, // the map the walk belongs to; Follow never goes above it
    scopePaths: new Map(), // component maps entered during the walk -> the paths they cover
    cameFrom: null, // component left by stepping out: the token starts there on the parent map
    movingSince: 0,
    playing: false,
    timer: 0,
  };
  const modal = $('#modal');
  modal.classList.add('walking');
  applyWalkWidth();
  modal.hidden = false;
  fit();
  showWalkStep(0);
}

function exitWalk() {
  if (!walker) return;
  clearTimeout(walker.timer);
  walker = null;
  stopSim();
  clearWalkMarks();
  document.querySelectorAll('#nodes .node.walk-current').forEach((el) => el.classList.remove('walk-current'));
  renderWalkBar();
  applyHighlight();
  $('#modal').classList.remove('walking');
  fit();
}

function clearWalkMarks() {
  document.querySelectorAll('#src-body .src-line.walk-range').forEach((el) => el.classList.remove('walk-range'));
  document.querySelectorAll('#src-body .walk-card').forEach((el) => el.remove());
}

async function showWalkStep(i, { auto = false } = {}) {
  if (!walker) return;
  if (!auto) pauseWalk();
  const { steps } = walker.walk;
  const index = Math.max(0, Math.min(steps.length - 1, i));
  walker.index = index;
  const step = steps[index];
  clearWalkMarks();
  markWalkNode({ animate: true });
  applyHighlight();
  renderWalkBar();
  syncWalkScope();
  if (!step.verified) {
    $('#modal').hidden = false;
    scheduleWalkNext();
    return;
  }
  await openSource(step.path, step.line);
  if (walker?.index !== index) return; // stepped again while the file loaded
  const tab = viewer.active;
  for (let n = step.line; n <= step.end_line; n++) lineEl(tab, n)?.classList.add('walk-range');
  const card = document.createElement('div');
  card.className = 'walk-card';
  card.innerHTML = walkCardHtml(step, index, steps.length);
  lineEl(tab, step.end_line)?.after(card);
  card.scrollIntoView({ block: 'nearest' });
  const first = lineEl(tab, step.line);
  if (first && first.getBoundingClientRect().top < $('#src-body').getBoundingClientRect().top) first.scrollIntoView({ block: 'start' });
  scheduleWalkNext();
}

// Autoplay: one step per interval, waiting for the token and any map change to finish.

const WALK_DELAYS = { slow: 6000, normal: 3500, fast: 1800 };

function walkSpeed() {
  const s = localStorage.getItem('cxplr:walk-speed');
  return WALK_DELAYS[s] ? s : 'normal';
}

function walkFollow() {
  return localStorage.getItem('cxplr:walk-follow') !== 'off';
}

/** A map change for the walk is still loading (give up waiting after a few seconds). */
function walkMoving(w) {
  return w.movingSince && performance.now() - w.movingSince < 5000;
}

function pauseWalk() {
  if (!walker) return;
  walker.playing = false;
  clearTimeout(walker.timer);
}

function toggleWalkPlay() {
  if (!walker) return;
  if (walker.playing) {
    pauseWalk();
    return renderWalkBar();
  }
  walker.playing = true;
  const atEnd = walker.index >= walker.walk.steps.length - 1;
  showWalkStep(atEnd ? 0 : walker.index + 1, { auto: true });
}

function scheduleWalkNext() {
  const w = walker;
  if (!w?.playing) return;
  clearTimeout(w.timer);
  const tick = () => {
    if (walker !== w || !w.playing) return;
    if (sim.running || walkMoving(w)) {
      w.timer = setTimeout(tick, 200);
      return;
    }
    if (w.index >= w.walk.steps.length - 1) {
      w.playing = false;
      return renderWalkBar();
    }
    showWalkStep(w.index + 1, { auto: true });
  };
  w.timer = setTimeout(tick, WALK_DELAYS[walkSpeed()]);
}

// Following the walk across map levels.

/**
 * Show the map level the current step's file belongs to: step out of a component map
 * the file is not in (never above the walk's own map) and, with Follow on, into the
 * map of the component the file is in, once the token has arrived.
 */
function syncWalkScope() {
  const w = walker;
  if (!w || walkMoving(w)) return;
  const step = w.walk.steps[w.index];
  if (!step.verified) return;
  const here = w.scopePaths.get(state.scope);
  if (here && !here.some((p) => pathMatch(p, step.path) >= 0)) return leaveWalkScope();
  if (!walkFollow()) return;
  const node = w.node && nodeById(w.node);
  if (node && hasChildMap(node.id) && node.paths.some((p) => pathMatch(p, step.path) > 0)) enterWalkScope(node.id, { afterToken: true });
}

function hasChildMap(id) {
  return ['fresh', 'stale'].includes(state.children?.[id]);
}

/** True when the shown map is below the walk's own map. */
function walkIsDeeper() {
  const base = walker.scope;
  return base ? state.scope.startsWith(`${base}/`) : state.scope !== '';
}

function enterWalkScope(id, { afterToken = false } = {}) {
  const w = walker;
  const node = nodeById(id);
  if (!node || !hasChildMap(id)) return;
  const target = childScope(id);
  w.scopePaths.set(target, node.paths);
  w.movingSince = performance.now();
  const t0 = performance.now();
  const goWhenArrived = () => {
    if (walker !== w) return;
    if (afterToken && sim.running && performance.now() - t0 < 3000) return setTimeout(goWhenArrived, 100);
    w.cameFrom = null;
    go(target);
  };
  goWhenArrived();
}

function leaveWalkScope() {
  const w = walker;
  if (!walkIsDeeper()) return;
  const cut = state.scope.lastIndexOf('/');
  w.cameFrom = state.scope.slice(cut + 1);
  w.movingSince = performance.now();
  go(cut < 0 ? '' : state.scope.slice(0, cut));
}

/** A new map is shown during the walk: re-fit, light the step's component there, follow on. */
function walkMapChanged() {
  const w = walker;
  w.movingSince = 0;
  // Stepping out: the token starts at the component that was left.
  w.node = w.cameFrom && state.layout.at[w.cameFrom] ? w.cameFrom : null;
  w.cameFrom = null;
  fit();
  markWalkNode({ animate: true });
  applyHighlight();
  renderWalkBar();
  syncWalkScope();
}

// Docked viewer width, adjustable by dragging its left edge.

function applyWalkWidth() {
  const saved = Number(localStorage.getItem('cxplr:walk-width'));
  $('#modal').style.setProperty('--walk-w', saved ? `${Math.max(420, Math.min(saved, window.innerWidth - 240))}px` : '');
}

$('#walk-resize').addEventListener('pointerdown', (e) => {
  if (!walker) return;
  e.preventDefault();
  const handle = e.currentTarget;
  handle.setPointerCapture(e.pointerId);
  const move = (ev) => {
    const width = Math.max(420, Math.min(window.innerWidth - 240, window.innerWidth - ev.clientX - 12));
    $('#modal').style.setProperty('--walk-w', `${width}px`);
    localStorage.setItem('cxplr:walk-width', String(Math.round(width)));
  };
  const up = () => {
    handle.removeEventListener('pointermove', move);
    handle.removeEventListener('pointerup', up);
    fit();
    const el = walker?.node && document.querySelector(`#nodes .node[data-node="${CSS.escape(walker.node)}"]`);
    if (el) keepNodeVisible(el);
  };
  handle.addEventListener('pointermove', move);
  handle.addEventListener('pointerup', up);
});

function walkCardHtml(step, index, total) {
  return `<div class="wc-head"><span class="wc-kind wc-${esc(step.kind)}">${esc(step.kind)}</span><b>${esc(step.title)}</b>
      <span class="muted">${index + 1} / ${total}</span></div>
    ${step.detail ? `<p>${esc(step.detail)}</p>` : ''}
    ${step.state ? `<pre class="payload">${esc(step.state)}</pre>` : ''}
    <div class="wc-nav"><button type="button" data-walk-prev ${index === 0 ? 'disabled' : ''}>◀ Back</button>
      <button type="button" class="primary" data-walk-next ${index === total - 1 ? 'disabled' : ''}>Next ▶</button>
      <span class="muted">[ / ] or K / J</span></div>`;
}

function renderWalkBar() {
  const bar = $('#walk-bar');
  if (!walker) {
    bar.hidden = true;
    bar.innerHTML = '';
    return;
  }
  const { walk, index, playing } = walker;
  const step = walk.steps[index];
  const meta = walk.meta || {};
  const node = walker.node && nodeById(walker.node);
  const speed = walkSpeed();
  bar.hidden = false;
  bar.innerHTML = `<button type="button" data-walk-prev ${index === 0 ? 'disabled' : ''} title="Previous step ([ or K)">◀</button>
    <span class="wb-count">${index + 1} / ${walk.steps.length}</span>
    <button type="button" data-walk-next ${index === walk.steps.length - 1 ? 'disabled' : ''} title="Next step (] or J)">▶</button>
    <button type="button" data-walk-play class="${playing ? 'active' : ''}" title="${playing ? 'Pause' : 'Play the steps one after another'} (Space)">${playing ? '❚❚ Pause' : '⏵ Play'}</button>
    <select id="walk-speed" title="Time per step while playing">${Object.keys(WALK_DELAYS).map((s) => `<option value="${s}" ${s === speed ? 'selected' : ''}>${s}</option>`).join('')}</select>
    <span class="wb-title"><b>${esc(meta.scenario || 'Walkthrough')}</b> <span class="muted">${esc(trunc(meta.input || '', 140))}</span></span>
    ${node ? `<span class="wb-node" title="The map component this code belongs to">${esc(node.name)}</span>` : ''}
    ${walkIsDeeper() ? '<button type="button" data-walk-out title="Back to the enclosing map">‹ Out</button>' : ''}
    ${node && hasChildMap(node.id) ? `<button type="button" data-walk-into title="Open ${esc(node.name)}'s own map and keep walking there">Into ${esc(trunc(node.name, 24))} ›</button>` : ''}
    <label class="wb-follow" title="Open a component's own map when the walk enters it, and leave it when the walk does"><input type="checkbox" id="walk-follow" ${walkFollow() ? 'checked' : ''}> Follow</label>
    ${step.verified ? '' : `<span class="error-text" title="${esc(step.title)}">${esc(step.path)}:${step.line}: ${esc(step.problem || 'not verified')}</span>`}
    <button type="button" data-walk-exit title="Leave the walkthrough (Esc)">✕</button>`;
}

/** How specifically a node path contains `path`: its length, 0 for the whole repository, -1 if not. */
function pathMatch(raw, path) {
  const p = raw.replace(/^\.\//, '').replace(/\/+$/, '');
  if (p === '' || p === '.') return 0;
  return path === p || path.startsWith(`${p}/`) ? p.length : -1;
}

/**
 * The node of the current map whose paths contain `path` most specifically. Nodes
 * sharing the file (e.g. a module's constants and its function) are told apart by
 * their citations in it: the one cited closest to `line` wins, preferring citations
 * at or above it (where the enclosing definition starts).
 */
function nodeForPath(path, line = 0) {
  let best = null;
  let bestLen = -1;
  let bestGap = Infinity;
  for (const n of state.map?.nodes || []) {
    if (isHidden(n.id)) continue;
    const len = Math.max(-1, ...n.paths.map((raw) => pathMatch(raw, path)));
    if (len < 0 || len < bestLen) continue;
    let gap = Infinity;
    for (const ev of n.evidence || []) {
      if (ev.path === path && line) gap = Math.min(gap, ev.line <= line ? line - ev.line : 2 * (ev.line - line));
    }
    if (len > bestLen || gap < bestGap) {
      best = n.id;
      bestLen = len;
      bestGap = gap;
    }
  }
  return best;
}

/** The component of a walkthrough step on the current map. */
function stepNode(s) {
  return s.verified ? nodeForPath(s.path, s.line) : null;
}

/** Component of step `i`: its file's component, or the last one before it. */
function walkNodeAt(i) {
  for (let k = i; k >= 0; k--) {
    const id = stepNode(walker.walk.steps[k]);
    if (id) return id;
  }
  return null;
}

/** Every step that enters another component: { index, from, to }. */
function walkHops() {
  const hops = [];
  let prev = null;
  walker.walk.steps.forEach((s, index) => {
    const id = stepNode(s);
    if (!id) return;
    if (prev && id !== prev) hops.push({ index, from: prev, to: id });
    prev = id;
  });
  return hops;
}

/**
 * Light up the current step's component. With `animate`, a step into another
 * component sends a token along the arrow between them (backwards when stepping back).
 */
function markWalkNode({ animate = false } = {}) {
  const id = walkNodeAt(walker.index);
  const from = walker.node;
  walker.node = id;
  document.querySelectorAll('#nodes .node.walk-current').forEach((el) => el.classList.remove('walk-current'));
  const el = id && document.querySelector(`#nodes .node[data-node="${CSS.escape(id)}"]`);
  if (!el) return;
  el.classList.add('walk-current');
  keepNodeVisible(el);
  if (!animate || from === id) return;
  if (from && state.layout.at[from]) {
    runSim([[{ from, to: id }]], { labels: [String(walker.index + 1)] });
  } else {
    el.classList.remove('sim-hit');
    void el.getBoundingClientRect(); // restart the animation
    el.classList.add('sim-hit');
  }
}

/** Map highlight while walking: arrows walked so far numbered by step, the rest dimmed. */
function applyWalkHighlight(nodes, edges, hiEdge, overlay) {
  const { map } = state;
  const onWalk = new Set(walker.walk.steps.map(stepNode).filter(Boolean));
  const walked = new Set();
  const upcoming = new Set();
  const badges = new Map();
  for (const h of walkHops()) {
    let i = map.edges.findIndex((e) => e.from === h.from && e.to === h.to);
    if (i < 0) i = map.edges.findIndex((e) => e.from === h.to && e.to === h.from);
    if (h.index > walker.index) {
      if (i >= 0) upcoming.add(i);
      continue;
    }
    let key;
    let mid;
    if (i >= 0) {
      walked.add(i);
      key = `e${i}`;
      mid = state.edgeGeom[i].mid;
    } else {
      // No arrow between the two components on this map: draw a dashed guide.
      key = `g${h.from}>${h.to}`;
      const g = edgePath(state.layout, h, 0);
      if (!badges.has(key)) overlay.insertAdjacentHTML('beforeend', `<path class="ghost" d="${g.d}" marker-end="url(#arrow-hi)"></path>`);
      mid = g.mid;
    }
    if (!badges.has(key)) badges.set(key, { mid, numbers: [] });
    badges.get(key).numbers.push(h.index + 1);
  }
  nodes.forEach((el) => { if (!onWalk.has(el.dataset.node)) el.classList.add('dim'); });
  edges.forEach((el, i) => {
    if (walked.has(i)) hiEdge(i);
    else if (!upcoming.has(i)) el.classList.add('dim');
  });
  drawStepBadges(overlay, badges);
}

/** Pan the map so `el` sits in the part not covered by the docked viewer. */
function keepNodeVisible(el) {
  const s = svg.getBoundingClientRect();
  const box = $('#modal').hidden ? null : $('.modal-box').getBoundingClientRect();
  const left = s.left + 16;
  const right = Math.min(s.right, box ? box.left : s.right) - 16;
  if (right - left < 120) return;
  const r = el.getBoundingClientRect();
  if (r.left >= left && r.right <= right && r.top >= s.top + 16 && r.bottom <= s.bottom - 16) return;
  state.view.x += (left + right) / 2 - (r.left + r.right) / 2;
  state.view.y += (s.top + s.bottom) / 2 - (r.top + r.bottom) / 2;
  applyView();
}

$('#modal').addEventListener('click', (e) => {
  if (!walker) return;
  if (e.target.closest('[data-walk-prev]')) return showWalkStep(walker.index - 1);
  if (e.target.closest('[data-walk-next]')) return showWalkStep(walker.index + 1);
  if (e.target.closest('[data-walk-exit]')) return exitWalk();
  if (e.target.closest('[data-walk-play]')) return toggleWalkPlay();
  if (e.target.closest('[data-walk-into]')) return walker.node && enterWalkScope(walker.node);
  if (e.target.closest('[data-walk-out]')) return leaveWalkScope();
});

$('#modal').addEventListener('change', (e) => {
  if (!walker) return;
  if (e.target.id === 'walk-follow') {
    localStorage.setItem('cxplr:walk-follow', e.target.checked ? 'on' : 'off');
    syncWalkScope();
  }
  if (e.target.id === 'walk-speed') {
    localStorage.setItem('cxplr:walk-speed', e.target.value);
    scheduleWalkNext();
  }
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
  sql: 'sql', graphql: 'graphql', diff: 'diff', patch: 'diff', mk: 'makefile', scala: 'scala', sc: 'scala', sbt: 'scala',
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

/**
 * The viewer keeps one tab per open file. A tab's pane stays rendered while other
 * tabs are shown, so switching back restores its scroll position and inline threads.
 * History is shared across tabs; each entry records where the view was when left.
 */
const viewer = {
  tabs: [],
  active: null,
  history: [],
  index: -1,
  outlineOpen: localStorage.getItem('cxplr:outline') !== 'off',
  get path() { return this.active?.path ?? null; },
  get lines() { return this.active?.lines ?? []; },
  get lsp() { return this.active?.lsp ?? null; },
};
const MAX_TABS = 20;
let tabSeq = 0;

function updateHistoryButtons() {
  $('#src-back').disabled = viewer.index <= 0;
  $('#src-fwd').disabled = viewer.index >= viewer.history.length - 1;
}

/** A tab's line element, 1-based. */
function lineEl(tab, n) {
  return tab?.lineEls?.[n - 1] || null;
}

function renderSrcPath() {
  const t = viewer.active;
  if (!t) { $('#src-path').innerHTML = ''; return; }
  const parts = t.path === '.' ? [] : t.path.split('/');
  $('#src-path').innerHTML = [`<a data-open=".">${esc(state.repo?.name || '.')}</a>`, ...parts.map((p, i) =>
    i === parts.length - 1 ? esc(p) : `<a data-open="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</a>`)].join(' / ') + (t.line ? `:${t.line}` : '');
}

function renderTabs() {
  $('#src-tabs').innerHTML = viewer.tabs.map((t) => {
    const name = t.path === '.' ? (state.repo?.name || '.') : t.path.split('/').pop();
    const notes = threads.filter((c) => c.path === t.path && !c.hidden).length;
    return `<div class="src-tab${t === viewer.active ? ' active' : ''}" role="tab" data-tab-id="${t.id}" title="${esc(t.path)}">` +
      `<span class="name">${esc(name)}${t.dir ? '/' : ''}</span>` +
      (notes ? `<span class="tab-dot" title="${notes} code thread${notes === 1 ? '' : 's'}">●</span>` : '') +
      `<button type="button" data-tab-close title="Close tab (middle-click)">✕</button></div>`;
  }).join('');
  $('#src-tabs .src-tab.active')?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
}

/** Record the scroll position of the location being left, so Back returns exactly there. */
function rememberScroll() {
  const t = viewer.active;
  if (!t) return;
  t.top = $('#src-body').scrollTop;
  const here = viewer.history[viewer.index];
  if (here?.path === t.path) here.top = t.top;
}

function activate(t) {
  const body = $('#src-body');
  if (viewer.active && viewer.active !== t) {
    viewer.active.top = body.scrollTop;
    viewer.active.el.hidden = true;
  }
  viewer.active = t;
  t.used = ++tabSeq;
  t.el.hidden = false;
  body.scrollTop = t.top;
  renderTabs();
  renderSrcPath();
  renderLspChip();
  renderOutline();
  runFind(false);
}

function newTab(path) {
  const el = document.createElement('div');
  el.className = 'src-pane';
  el.hidden = true;
  $('#src-body').append(el);
  const t = { id: `tab${++tabSeq}`, used: tabSeq, path, line: 0, el, lines: [], lineEls: [], dir: false, lsp: null, outline: [], outlineFrom: null, lspOutline: false, top: 0 };
  viewer.tabs.push(t);
  if (viewer.tabs.length > MAX_TABS) {
    const stale = viewer.tabs.filter((x) => x !== t && x !== viewer.active).sort((a, b) => a.used - b.used)[0];
    if (stale) closeTab(stale);
  }
  return t;
}

function closeTab(t) {
  const i = viewer.tabs.indexOf(t);
  if (i < 0) return;
  viewer.tabs.splice(i, 1);
  t.el.remove();
  if (viewer.active !== t) return renderTabs();
  viewer.active = null;
  const next = viewer.tabs[Math.min(i, viewer.tabs.length - 1)];
  if (next) return openSource(next.path, next.line, { top: next.top });
  viewer.history = [];
  viewer.index = -1;
  closeSource();
}

/** Fetch and render a tab's file or directory listing. */
async function loadTab(t) {
  t.el.innerHTML = '<p class="muted" style="padding:16px">Loading…</p>';
  let res;
  try {
    res = await api(`/api/source?path=${encodeURIComponent(t.path)}`);
  } catch (err) {
    t.el.innerHTML = `<div class="src-error">${esc(err.message)}</div>`;
    return;
  }
  if (res.dir) {
    t.dir = true;
    const base = res.path === '.' ? '' : `${res.path}/`;
    t.el.innerHTML = '<ul class="dir-list">' + res.entries.map((e) =>
      `<li><a data-open="${esc(base + e.name)}">${esc(e.name)}${e.dir ? '/' : ''}</a></li>`).join('') + '</ul>';
    renderTabs();
    return;
  }
  const text = res.text.endsWith('\n') ? res.text.slice(0, -1) : res.text;
  t.lines = text.split('\n');
  const html = highlightLines(text, t.path);
  t.el.innerHTML = `<div class="src-hint">Click a name for its definition, documentation and references · Ctrl/⌘-click jumps to the definition · Ctrl/⌘+F finds · Alt+←/→ history</div>` +
    html.map((h, i) => `<div class="src-line" data-line="${i + 1}"><span class="no">${i + 1}</span><span class="code">${h || ' '}</span></div>`).join('');
  t.lineEls = [...t.el.querySelectorAll('.src-line')];
  t.outline = guessOutline(t.lines, t.path);
  t.outlineFrom = 'patterns';
  refreshLspStatus(t, true);
}

function reveal(t, line, top) {
  t.el.querySelectorAll('.src-line.target').forEach((el) => el.classList.remove('target'));
  const el = lineEl(t, line);
  el?.classList.add('target');
  if (top != null) $('#src-body').scrollTop = top;
  else el?.scrollIntoView({ block: 'center' });
}

/**
 * Show a file (or directory) at a 1-based line in its tab, opening the tab if needed.
 * `top` restores an exact scroll position (history); otherwise the line is centred.
 */
async function openSource(path, line, { push = true, top = null } = {}) {
  if (push) {
    rememberScroll();
    viewer.history = viewer.history.slice(0, viewer.index + 1);
    viewer.history.push({ path, line });
    viewer.index = viewer.history.length - 1;
  }
  updateHistoryButtons();
  $('#modal').hidden = false;
  $('#sym').hidden = true;
  let t = viewer.tabs.find((x) => x.path === path);
  const fresh = !t;
  if (fresh) t = newTab(path);
  t.line = line;
  activate(t);
  if (fresh) {
    await loadTab(t);
    if (viewer.active !== t) return;
    renderOutline();
  }
  threads.filter((c) => c.path === path).forEach(mountThread);
  reveal(t, line, top);
  if (fresh) runFind(false);
}

async function refreshLspStatus(t, start) {
  let status;
  try {
    status = await api(`/api/lsp/status?path=${encodeURIComponent(t.path)}${start ? '&start=true' : ''}`);
  } catch {
    status = { state: 'none' };
  }
  if (!viewer.tabs.includes(t)) return;
  t.lsp = status;
  if (viewer.active === t) renderLspChip();
  if (['idle', 'starting', 'indexing'].includes(status.state)) setTimeout(() => refreshLspStatus(t, false), 1500);
  if (['ready', 'indexing'].includes(status.state) && !t.lspOutline) loadLspOutline(t);
}

function renderLspChip() {
  const status = viewer.active?.lsp;
  const chip = $('#src-lsp');
  chip.hidden = !status;
  if (!status) return;
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

/** Text matches across the repository into the symbol pane: whole words, or smart-case substrings. */
async function textSearch(query, { word = true } = {}) {
  const box = $('#sym-text');
  if (!box) return;
  box.innerHTML = '<p class="muted">Searching…</p>';
  try {
    const res = await api(`/api/search?q=${encodeURIComponent(query)}${word ? '&word=true' : ''}&limit=${word ? 300 : 500}`);
    box.innerHTML = `<h3>Text matches (${res.hits.length}${res.truncated ? '+' : ''})</h3>` +
      locItems(res.hits.map((h) => ({ ...h, display: h.path })), { group: true });
  } catch (err) {
    box.innerHTML = `<p class="src-error" style="padding:0">${esc(err.message)}</p>`;
  }
}

function repoSearch(query) {
  if (!query.trim()) return;
  const pane = $('#sym');
  pane.hidden = false;
  const exact = /\p{Lu}/u.test(query);
  pane.innerHTML = `<div class="sym-head"><code>${esc(query)}</code><span class="spacer"></span><button type="button" data-sym-close title="Close">✕</button></div>` +
    `<p class="muted small">Across the repository, ${exact ? 'matching case (the query has capitals)' : 'ignoring case'}. Opened files highlight the matches.</p><div id="sym-text"></div>`;
  textSearch(query, { word: false });
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
  rememberScroll();
  viewer.index = i;
  const { path, line, top } = viewer.history[i];
  openSource(path, line, { push: false, top });
}
$('#src-back').onclick = () => historyStep(-1);
$('#src-fwd').onclick = () => historyStep(1);

$('#src-tabs').addEventListener('click', (e) => {
  const el = e.target.closest('.src-tab');
  const t = el && viewer.tabs.find((x) => x.id === el.dataset.tabId);
  if (!t) return;
  if (e.target.closest('[data-tab-close]')) return closeTab(t);
  if (t !== viewer.active) openSource(t.path, t.line, { top: t.top });
});
$('#src-tabs').addEventListener('auxclick', (e) => {
  const el = e.button === 1 && e.target.closest('.src-tab');
  const t = el && viewer.tabs.find((x) => x.id === el.dataset.tabId);
  if (t) { e.preventDefault(); closeTab(t); }
});

/** Hide the viewer; tabs, their scroll positions and threads stay for the next file opened. */
function closeSource() {
  exitWalk();
  rememberScroll();
  $('#modal').hidden = true;
  $('#sym').hidden = true;
  selAsk.hidden = true;
}

// ---------- outline ----------

/** Declarations by language for files without a language server: [pattern, kind]; a `kind` group overrides. */
const MODIFIERS = '(?:(?:public|private|protected|internal|static|final|abstract|sealed|open|data|inner|partial|override|virtual|async|export|default|suspend|inline|mutating)\\s+)*';
const OUTLINE_RULES = {
  rust: [
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?(?:(?:const|async|unsafe|extern(?:\s+"[^"]*")?)\s+)*fn\s+(?<name>[A-Za-z_]\w*)/, 'function'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?(?<kind>struct|enum|trait|union|mod|type)\s+(?<name>[A-Za-z_]\w*)/],
    [/^\s*(?:unsafe\s+)?impl\b(?:\s*<[^{]*?>)?\s+(?<name>[^{]+?)\s*(?:\{|\bwhere\b|$)/, 'impl'],
    [/^\s*macro_rules!\s*(?<name>\w+)/, 'macro'],
  ],
  python: [
    [/^\s*(?:async\s+)?def\s+(?<name>\w+)/, 'function'],
    [/^\s*class\s+(?<name>\w+)/, 'class'],
  ],
  js: [
    [/^\s*(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s*\*?\s*(?<name>[\w$]+)/, 'function'],
    [/^\s*(?:export\s+(?:default\s+)?)?(?:declare\s+)?(?:abstract\s+)?(?<kind>class|interface|enum|namespace)\s+(?<name>[\w$]+)/],
    [/^\s*(?:export\s+)?(?:declare\s+)?type\s+(?<name>[\w$]+)\s*(?:<[^=]*>)?\s*=/, 'type'],
    [/^\s*(?:export\s+)?(?:const|let|var)\s+(?<name>[\w$]+)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[\w$]+\s*=>)/, 'function'],
    [/^\s+(?:(?:public|private|protected|static|async|readonly|override|abstract|get|set)\s+)*(?<name>#?[\w$]+)\s*(?:<[^>]*>)?\s*\([^)]*\)\s*(?::\s*[^{]+)?\{\s*$/, 'method'],
  ],
  go: [
    [/^func\s+\([^)]*\)\s*(?<name>\w+)/, 'method'],
    [/^func\s+(?<name>\w+)/, 'function'],
    [/^type\s+(?<name>\w+)\s+(?<kind>struct|interface)\b/],
    [/^type\s+(?<name>\w+)\b/, 'type'],
  ],
  jvm: [
    [new RegExp(`^\\s*${MODIFIERS}(?<kind>class|interface|enum|struct|record|object|trait|protocol|extension)\\s+(?<name>\\w+)`)],
    [new RegExp(`^\\s*${MODIFIERS}(?:fun|func|def)\\s+(?:<[^>]*>\\s*)?(?:[\\w.]+\\.)?(?<name>\\w+)`), 'function'],
    [/^\s+(?:(?:public|private|protected|internal|static|final|abstract|synchronized|override|virtual|async|native|default)\s+)+[\w<>[\],.?\s]+?\s+(?<name>\w+)\s*\([^;]*$/, 'method'],
  ],
  scala: [
    [/^\s*(?:(?:private|protected)(?:\[[\w.]*\])?\s+|(?:final|sealed|abstract|implicit|case|lazy|override|inline|transparent|open|opaque)\s+)*(?<kind>class|trait|object|enum)\s+(?<name>\w+)/],
    [/^\s*(?:(?:private|protected)(?:\[[\w.]*\])?\s+|(?:final|implicit|override|inline|transparent|opaque)\s+)*(?<kind>type|given)\s+(?<name>\w+)/],
    [/^\s*(?:(?:private|protected)(?:\[[\w.]*\])?\s+|(?:final|implicit|override|inline|transparent|abstract)\s+)*def\s+(?<name>[\w$]+|`[^`]+`|[^\s\w(\[:]+)/, 'method'],
    [/^\s*extension\s*(?:\[[^\]]*\])?\s*\((?<name>[^)]*)\)/, 'extension'],
    [/^package\s+(?<name>[\w.]+)/, 'package'],
  ],
  c: [
    [/^\s*(?:typedef\s+)?(?<kind>struct|class|enum|union|namespace)\s+(?<name>\w+)\s*(?:[:{]|$)/],
    [/^[A-Za-z_][\w:<>,*&\s]*?[\s*&](?<name>[A-Za-z_][\w:~]*)\s*\([^;]*$/, 'function'],
  ],
  ruby: [
    [/^\s*def\s+(?:self\.)?(?<name>[\w?!=]+)/, 'method'],
    [/^\s*(?<kind>class|module)\s+(?<name>[\w:]+)/],
  ],
  php: [
    [/^\s*(?:(?:public|private|protected|static|abstract|final)\s+)*function\s+(?<name>\w+)/, 'function'],
    [/^\s*(?:abstract\s+|final\s+)?(?<kind>class|interface|trait|enum)\s+(?<name>\w+)/],
  ],
  shell: [
    [/^\s*function\s+(?<name>[\w.:-]+)/, 'function'],
    [/^\s*(?<name>[\w.:-]+)\s*\(\)\s*\{?/, 'function'],
  ],
};
const OUTLINE_LANG = {
  rs: 'rust', py: 'python', pyi: 'python', js: 'js', mjs: 'js', cjs: 'js', jsx: 'js', ts: 'js', tsx: 'js', mts: 'js', cts: 'js',
  go: 'go', java: 'jvm', kt: 'jvm', kts: 'jvm', scala: 'scala', sc: 'scala', sbt: 'scala', cs: 'jvm', swift: 'jvm', dart: 'jvm',
  c: 'c', h: 'c', cc: 'c', cpp: 'c', cxx: 'c', hpp: 'c', hh: 'c', rb: 'ruby', php: 'php',
  sh: 'shell', bash: 'shell', zsh: 'shell', md: 'markdown', markdown: 'markdown',
};
const OUTLINE_KEYWORDS = new Set(['if', 'else', 'for', 'while', 'switch', 'catch', 'return', 'function', 'do', 'new', 'sizeof', 'typeof', 'delete', 'throw', 'case', 'match', 'with']);
const COMMENT_LINE = /^\s*(?:\/\/|\/\*|\*(?:\s|$)|#)/;

/** Outline from name patterns; nesting follows indentation (Markdown: heading level). */
function guessOutline(lines, path) {
  const lang = OUTLINE_LANG[path.split('/').pop().split('.').pop().toLowerCase()];
  const items = [];
  if (lang === 'markdown') {
    let fence = false;
    lines.forEach((text, i) => {
      if (/^\s*(```|~~~)/.test(text)) fence = !fence;
      const m = !fence && text.match(/^(#{1,6})\s+(.+?)\s*#*\s*$/);
      if (m) items.push({ name: m[2], kind: 'heading', line: i + 1, depth: m[1].length - 1 });
    });
    return items;
  }
  const rules = OUTLINE_RULES[lang];
  if (!rules) return items;
  const indents = [];
  lines.forEach((text, i) => {
    if (COMMENT_LINE.test(text)) return;
    for (const [re, kind] of rules) {
      const m = text.match(re);
      const name = m?.groups.name?.trim();
      if (!name || OUTLINE_KEYWORDS.has(name)) continue;
      const indent = text.match(/^[ \t]*/)[0].replace(/\t/g, '    ').length;
      while (indents.length && indents[indents.length - 1] >= indent) indents.pop();
      const k = m.groups.kind || kind;
      items.push({ name, kind: k === 'mod' ? 'module' : k, line: i + 1, depth: indents.length });
      indents.push(indent);
      break;
    }
  });
  return items;
}

async function loadLspOutline(t) {
  t.lspOutline = true;
  let symbols;
  try {
    ({ symbols } = await api(`/api/lsp/outline?path=${encodeURIComponent(t.path)}`));
  } catch {
    return; // the pattern outline stays, labelled as such
  }
  if (!symbols.length && t.outline.length) return;
  t.outline = symbols;
  t.outlineFrom = 'lsp';
  if (viewer.active === t) renderOutline();
}

const OUTLINE_BADGES = {
  function: ['ƒ', 'fn'], method: ['ƒ', 'fn'], constructor: ['ƒ', 'fn'], macro: ['!', 'fn'],
  class: ['C', 'ty'], struct: ['S', 'ty'], union: ['S', 'ty'], record: ['S', 'ty'], object: ['O', 'ty'], impl: ['⧉', 'ty'],
  enum: ['E', 'ty'], interface: ['I', 'ty'], trait: ['I', 'ty'], protocol: ['I', 'ty'], extension: ['+', 'ty'], type: ['T', 'ty'],
  module: ['M', 'mod'], namespace: ['M', 'mod'], package: ['M', 'mod'],
  variable: ['v', 'val'], constant: ['c', 'val'], heading: ['#', 'doc'],
};

function renderOutline() {
  const t = viewer.active;
  const nav = $('#outline');
  $('#src-outline-btn').classList.toggle('active', viewer.outlineOpen);
  nav.hidden = !viewer.outlineOpen || !t || t.dir;
  if (nav.hidden) return;
  const filter = $('#ol-filter').value.trim().toLowerCase();
  const items = t.outline.filter((s) => !filter || s.name.toLowerCase().includes(filter));
  $('#ol-list').innerHTML = items.map((s) => {
    const [badge, group] = OUTLINE_BADGES[s.kind] || ['·', 'val'];
    const title = `${s.kind}${s.detail ? ` · ${s.detail}` : ''} · line ${s.line}`;
    return `<li data-line="${s.line}" style="padding-left:${6 + Math.min(s.depth, 8) * 12}px" title="${esc(title)}">` +
      `<span class="ol-badge ol-${group}">${badge}</span><span class="ol-name">${esc(s.name)}</span></li>`;
  }).join('');
  const from = t.outlineFrom === 'lsp' ? 'language server' : 'name patterns';
  $('#ol-foot').textContent = !t.lines.length ? ''
    : !t.outline.length ? (t.outlineFrom === 'lsp' ? 'No declarations in this file.' : 'No functions or classes recognised.')
    : `${t.outline.length} symbol${t.outline.length === 1 ? '' : 's'} · ${from}${filter ? ` · ${items.length} shown` : ''}`;
  markCurrentSymbol();
}

/** The first source line visible at the top of the viewer. */
function firstVisibleLine() {
  const r = $('#src-body').getBoundingClientRect();
  const el = document.elementFromPoint(r.left + 80, r.top + 30)?.closest('.src-line');
  return Number(el?.dataset.line) || 1;
}

/** Mark the declaration the view is in. */
function markCurrentSymbol() {
  const t = viewer.active;
  if ($('#outline').hidden || !t?.outline.length) return;
  const top = firstVisibleLine();
  let current = 0;
  for (const s of t.outline) if (s.line <= top + 1) current = Math.max(current, s.line);
  $('#ol-list').querySelectorAll('li').forEach((li) => {
    const on = Number(li.dataset.line) === current;
    li.classList.toggle('current', on);
    if (on) li.scrollIntoView({ block: 'nearest' });
  });
}
let outlineFrame = 0;
$('#src-body').addEventListener('scroll', () => {
  cancelAnimationFrame(outlineFrame);
  outlineFrame = requestAnimationFrame(markCurrentSymbol);
});

$('#ol-list').addEventListener('click', (e) => {
  const li = e.target.closest('li[data-line]');
  if (li && viewer.path) openSource(viewer.path, Number(li.dataset.line));
});
$('#ol-filter').addEventListener('input', renderOutline);
$('#ol-filter').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') { const li = $('#ol-list li[data-line]'); if (li) openSource(viewer.path, Number(li.dataset.line)); }
  if (e.key === 'Escape' && e.target.value) { e.stopPropagation(); e.target.value = ''; renderOutline(); }
});
$('#src-outline-btn').onclick = () => {
  viewer.outlineOpen = !viewer.outlineOpen;
  localStorage.setItem('cxplr:outline', viewer.outlineOpen ? 'on' : 'off');
  renderOutline();
};

// ---------- find in file ----------

const MAX_FIND = 5000;
const find = { q: '', matches: [], ranges: [], index: -1 };

/** Smart case: case-insensitive unless the query has capitals. */
function findMatches(lines, q) {
  const fold = !/\p{Lu}/u.test(q);
  const needle = fold ? q.toLowerCase() : q;
  const out = [];
  for (let i = 0; i < lines.length && out.length < MAX_FIND; i++) {
    const hay = fold ? lines[i].toLowerCase() : lines[i];
    for (let at = hay.indexOf(needle); at >= 0 && out.length < MAX_FIND; at = hay.indexOf(needle, at + needle.length)) {
      out.push({ line: i + 1, start: at, end: at + needle.length });
    }
  }
  return out;
}

/** A DOM range over UTF-16 offsets of a highlighted line's text. */
function textRange(codeEl, start, end) {
  const range = document.createRange();
  const walker = document.createTreeWalker(codeEl, NodeFilter.SHOW_TEXT);
  let pos = 0;
  let started = false;
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const len = n.nodeValue.length;
    if (!started && start < pos + len) { range.setStart(n, start - pos); started = true; }
    if (started && end <= pos + len) { range.setEnd(n, end - pos); return range; }
    pos += len;
  }
  return null;
}

/** Recompute matches in the active tab; `jump` moves to the first match from the top of the view. */
function runFind(jump) {
  const t = viewer.active;
  find.q = $('#find-q').value;
  find.matches = t && find.q ? findMatches(t.lines, find.q) : [];
  find.ranges = find.matches.map((m) => {
    const code = lineEl(t, m.line)?.querySelector('.code');
    return code && textRange(code, m.start, m.end);
  });
  if (window.CSS?.highlights) CSS.highlights.set('find', new Highlight(...find.ranges.filter(Boolean)));
  const from = jump ? firstVisibleLine() : (t?.line || firstVisibleLine());
  find.index = find.matches.length ? Math.max(0, find.matches.findIndex((m) => m.line >= from)) : -1;
  showFindMatch(jump);
}

function showFindMatch(scroll) {
  const n = find.matches.length;
  $('#find-count').textContent = !find.q ? '' : n ? `${find.index + 1}/${n}${n >= MAX_FIND ? '+' : ''}` : 'no matches';
  $('#find-q').classList.toggle('none', Boolean(find.q) && !n);
  document.querySelectorAll('#src-body .src-line.find-line').forEach((el) => el.classList.remove('find-line'));
  const m = find.matches[find.index];
  const el = m && lineEl(viewer.active, m.line);
  el?.classList.add('find-line');
  if (window.CSS?.highlights) {
    const current = new Highlight(...(find.ranges[find.index] ? [find.ranges[find.index]] : []));
    current.priority = 1;
    CSS.highlights.set('find-current', current);
  }
  if (scroll) el?.scrollIntoView({ block: 'center' });
}

function stepFind(delta) {
  if ($('#find-q').value !== find.q) return runFind(true);
  const n = find.matches.length;
  if (!n) return;
  find.index = (find.index + delta + n) % n;
  showFindMatch(true);
}

let findTimer = 0;
$('#find-q').addEventListener('input', () => { clearTimeout(findTimer); findTimer = setTimeout(() => runFind(true), 120); });
$('#find-q').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && e.altKey) { e.preventDefault(); return repoSearch(e.target.value); }
  if (e.key === 'Enter') { e.preventDefault(); return stepFind(e.shiftKey ? -1 : 1); }
  if (e.key === 'Escape') {
    e.stopPropagation();
    if (e.target.value) { e.target.value = ''; runFind(false); } else e.target.blur();
  }
});
$('#find-prev').onclick = () => stepFind(-1);
$('#find-next').onclick = () => stepFind(1);
$('#find-repo').onclick = () => repoSearch($('#find-q').value);

/** Ctrl/⌘+F: focus the find box, seeded with a one-line selection. */
function focusFind() {
  const box = $('#find-q');
  const sel = String(window.getSelection() || '');
  if (sel && !sel.includes('\n') && sel.length <= 200) { box.value = sel; runFind(false); }
  box.focus();
  box.select();
}

// ---------- inline code threads ----------

/**
 * A conversation is the Ask tab's, or a thread under selected code lines. Each
 * keeps its own agent session: the first question forks the current map's
 * session, follow-ups continue it.
 */
function conversation(id, extra) {
  return { id, entries: [], session: null, ...extra };
}
const mainConv = conversation('main', { render: () => { if (state.tab === 'ask') renderChat(); updateAskBusy(); } });
const threads = []; // conversations with { path, start, end, hidden }
let threadSeq = 0;

function convById(id) {
  return id === 'main' ? mainConv : threads.find((t) => t.id === id);
}

function threadLabel(t) {
  return t.start ? `${t.path}:${t.start}${t.end !== t.start ? `–${t.end}` : ''}` : t.path;
}

function threadEl(t) {
  const el = document.createElement('div');
  el.className = 'inline-thread';
  el.dataset.thread = t.id;
  el.innerHTML = `<div class="it-head"><b>${esc(t.start ? `Lines ${t.start}–${t.end}` : 'Whole file')}</b>
      <span class="muted it-state"></span><span class="spacer"></span>
      <button type="button" data-it-hide title="Hide (the thread stays in the Ask tab)">✕</button></div>
    <div class="it-entries"></div>
    <div class="it-compose"><textarea rows="2" placeholder="Ask about these lines… (Ctrl+Enter)"></textarea>
      <button type="button" class="primary" data-it-send>Ask</button></div>`;
  return el;
}

/** Place a thread in its file's tab: under its last line, or at the top for the whole file. */
function mountThread(t) {
  const tab = viewer.tabs.find((x) => x.path === t.path);
  if (!tab?.lines.length || t.hidden) return;
  let el = tab.el.querySelector(`.inline-thread[data-thread="${t.id}"]`);
  if (!el) {
    el = threadEl(t);
    const anchor = t.end ? lineEl(tab, t.end) : tab.el.querySelector('.src-hint');
    anchor?.after(el);
  }
  renderThread(t);
  return el;
}

function renderThread(t) {
  updateAskBusy();
  if (state.tab === 'ask') renderChat();
  const el = $(`#src-body .inline-thread[data-thread="${t.id}"]`);
  if (!el) return;
  $('.it-entries', el).innerHTML = t.entries.map((e) => renderEntry(e, t.id)).join('');
  $('.it-state', el).textContent = t.session ? 'conversation continues' : '';
  $('[data-it-send]', el).disabled = t.entries.some((e) => e.status === 'running');
}

function openThread(path, start, end) {
  let t = threads.find((x) => x.path === path && x.start === start && x.end === end);
  if (!t) {
    t = conversation(`t${++threadSeq}`, { path, start, end, hidden: false });
    t.render = () => renderThread(t);
    threads.push(t);
  }
  t.hidden = false;
  const el = mountThread(t);
  renderTabs();
  el?.scrollIntoView({ block: 'nearest' });
  $('textarea', el)?.focus();
}

function threadContext(t) {
  return { ctx: { scope: state.scope, path: t.path, start_line: t.start, end_line: t.end }, label: threadLabel(t) };
}

/** Lines selected in the viewer (1-based, inclusive), or null. */
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
  openThread(viewer.path, range?.[0] ?? null, range?.[1] ?? null);
};

// Selecting code offers an inline question right where the selection ends.
const selAsk = $('#sel-ask');
$('#src-body').addEventListener('mouseup', (e) => {
  if (e.target.closest('.inline-thread')) return;
  setTimeout(() => {
    const range = selectedLines();
    if (!range || !viewer.lines.length) { selAsk.hidden = true; return; }
    selAsk.textContent = range[0] === range[1] ? `Ask about line ${range[0]}` : `Ask about lines ${range[0]}–${range[1]}`;
    selAsk.dataset.start = range[0];
    selAsk.dataset.end = range[1];
    selAsk.style.left = `${Math.min(e.clientX + 10, window.innerWidth - 220)}px`;
    selAsk.style.top = `${e.clientY + 12}px`;
    selAsk.hidden = false;
  }, 0);
});
document.addEventListener('mousedown', (e) => { if (e.target !== selAsk) selAsk.hidden = true; });
$('#src-body').addEventListener('scroll', () => { selAsk.hidden = true; });
selAsk.onclick = () => {
  selAsk.hidden = true;
  openThread(viewer.path, Number(selAsk.dataset.start), Number(selAsk.dataset.end));
  window.getSelection()?.removeAllRanges();
};

function sendThreadQuestion(el) {
  const t = convById(el.dataset.thread);
  const box = $('textarea', el);
  const question = box.value.trim();
  if (!t || !question || t.entries.some((x) => x.status === 'running')) return;
  box.value = '';
  const { ctx, label } = threadContext(t);
  sendQuestion(t, question, ctx, label);
}

$('#src-body').addEventListener('click', (e) => {
  const el = e.target.closest('.inline-thread');
  if (!el) return;
  if (e.target.closest('[data-it-send]')) return sendThreadQuestion(el);
  if (e.target.closest('[data-it-hide]')) {
    convById(el.dataset.thread).hidden = true;
    el.remove();
    if (state.tab === 'ask') renderChat();
    renderTabs();
    return;
  }
  conversationClick(e);
});
$('#src-body').addEventListener('keydown', (e) => {
  const el = e.target.closest('.inline-thread');
  if (el && e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); sendThreadQuestion(el); }
});

$('#modal').addEventListener('click', (e) => {
  const open = e.target.closest('[data-open]');
  if (open) return openSource(open.dataset.open, 0);
  if (e.target.id === 'modal') closeSource();
});
$('#src-close').onclick = closeSource;

// ---------- ask ----------


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
  chips.push(mainConv.session
    ? `<span class="chip" title="Follow-ups continue ${esc(engineName('ask'))} session ${esc(mainConv.session)}">Conversation <b>continues</b><button type="button" data-new-conv title="Start a new conversation">✕</button></span>`
    : `<span class="chip" title="${state.repo?.ask_seeded ? `The first question starts a new ${esc(engineName('ask'))} session seeded with the atlas` : 'The first question forks the session that explored this map'}">Conversation <b>new</b></span>`);
  const node = state.selected?.type === 'node' ? nodeById(state.selected.id) : null;
  if (node) {
    ctx.node = node.id;
    chips.push(`<span class="chip">Component <b>${esc(node.name)}</b></span>`);
  }
  return { ctx, chips, label: [crumbs, node?.name].filter(Boolean).join(' · ') };
}

function renderAskContext() {
  $('#ask-context').innerHTML = askContext().chips.join('');
}

$('#ask-context').addEventListener('click', (e) => {
  if (e.target.closest('[data-new-conv]')) { mainConv.session = null; renderAskContext(); }
});

function renderEntry(entry, convId) {
  const q = `<div class="msg-q">${esc(entry.question)}</div><div class="msg-ctx">${esc(entry.label)}</div>`;
  if (entry.status === 'running') {
    return q + `<div class="msg-a"><div class="status"><div class="spinner"></div><span>${esc(engineName('ask'))} is investigating… ${fmtDuration(entry.elapsed || 0)}</span>
      <button type="button" data-cancel="${entry.job}">Cancel</button></div>${usageHtml(entry.usage)}${entry.last ? `<div class="last">${esc(entry.last)}</div>` : ''}</div>`;
  }
  if (entry.status !== 'done') {
    return q + `<div class="msg-a"><div class="error">${esc(entry.error || entry.status)}</div>
      <div class="actions"><button type="button" data-retry="${convId}:${entry.id}">Ask again</button></div></div>`;
  }
  const r = entry.result;
  const cites = r.citations.length ? `<h4 class="md-h">Citations</h4>${evidenceList(r.citations)}` : '';
  return q + `<div class="msg-a">${mdBlock(r.answer)}${cites}
    <p class="meta">${fmtDuration(r.duration_ms)} · ${r.commands} commands · ${fmtTokens(r.input_tokens)} in (${fmtTokens(r.cached_input_tokens || 0)} cached) / ${fmtTokens(r.output_tokens)} out tokens</p></div>`;
}

function updateAskBusy() {
  $('#ask-busy').hidden = ![mainConv, ...threads].some((c) => c.entries.some((e) => e.status === 'running'));
}

function renderChat() {
  const chat = $('#chat');
  const stick = chat.scrollTop + chat.clientHeight >= chat.scrollHeight - 40;
  const list = threads.length ? `<div class="thread-list"><h4 class="md-h">Code threads</h4><ul class="list">${threads.map((t) => {
    const last = t.entries[t.entries.length - 1];
    return `<li class="click" data-open-thread="${t.id}"><a class="loc">${esc(threadLabel(t))}</a>
      <span class="muted"> · ${t.entries.length} question${t.entries.length === 1 ? '' : 's'}${t.entries.some((e) => e.status === 'running') ? ' · answering…' : ''}</span>
      ${last ? `<div class="muted">${esc(trunc(last.question, 90))}</div>` : ''}</li>`;
  }).join('')}</ul></div>` : '';
  const main = mainConv.entries.length ? mainConv.entries.map((e) => renderEntry(e, 'main')).join('') : `<div class="chat-empty">
    <p>Ask anything about this repository. The question includes what you are looking at: the current map and the selected component.</p>
    <p>To ask about code, select lines in the source viewer and use <b>Ask about lines…</b>; the thread opens right under the code.</p>
    <p>Answers cite <code>file:line</code>; click a citation to open it.</p></div>`;
  chat.innerHTML = list + main;
  if (stick) chat.scrollTop = chat.scrollHeight;
  updateAskBusy();
}

/** Ask in a conversation; its session continues once the first answer returns it. */
async function sendQuestion(conv, question, ctx, label) {
  const entry = { id: conv.entries.length, question, label, ctx, status: 'running', elapsed: 0, last: '' };
  conv.entries.push(entry);
  conv.render();
  try {
    const res = await post('/api/ask', { question, context: ctx, session: conv.session });
    entry.job = res.job;
  } catch (err) {
    Object.assign(entry, { status: 'failed', error: err.message });
    return conv.render();
  }
  let since = 0;
  const tick = async () => {
    let job;
    try {
      job = await api(`/api/job/${entry.job}?since=${since}`);
    } catch (err) {
      Object.assign(entry, { status: 'failed', error: err.message });
      return conv.render();
    }
    since = job.next;
    const lines = job.progress.filter((l) => l.startsWith('$ ') || l.startsWith('› '));
    if (lines.length) entry.last = lines[lines.length - 1];
    entry.elapsed = job.elapsed_ms;
    entry.usage = job.usage;
    if (job.status === 'running') {
      setTimeout(tick, 1000);
    } else if (job.status === 'done') {
      Object.assign(entry, { status: 'done', result: job.result });
      conv.session = job.result.session;
      if (conv === mainConv) renderAskContext();
    } else {
      Object.assign(entry, { status: job.status, error: job.status === 'cancelled' ? 'Cancelled.' : job.error });
    }
    conv.render();
  };
  tick();
}

function submitQuestion() {
  const box = $('#question');
  const question = box.value.trim();
  // One question at a time keeps the conversation's session linear.
  if (!question || mainConv.entries.some((e) => e.status === 'running')) return;
  box.value = '';
  const { ctx, label } = askContext();
  sendQuestion(mainConv, question, ctx, label);
  $('#chat').scrollTop = $('#chat').scrollHeight;
}

$('#ask-send').onclick = submitQuestion;
$('#question').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); submitQuestion(); }
});

/** Citations, cancel and retry inside any conversation. */
function conversationClick(e) {
  const loc = e.target.closest('[data-src]');
  if (loc) return openSource(loc.dataset.src, Number(loc.dataset.line) || 0);
  const cancel = e.target.closest('[data-cancel]');
  if (cancel) { cancel.disabled = true; return post(`/api/job/${cancel.dataset.cancel}/cancel`).catch(() => {}); }
  const retry = e.target.closest('[data-retry]');
  if (retry) {
    const [convId, n] = retry.dataset.retry.split(':');
    const conv = convById(convId);
    const old = conv?.entries[Number(n)];
    if (old && !conv.entries.some((x) => x.status === 'running')) sendQuestion(conv, old.question, old.ctx, old.label);
  }
}

$('#chat').addEventListener('click', (e) => {
  const open = e.target.closest('[data-open-thread]');
  if (open) {
    const t = convById(open.dataset.openThread);
    t.hidden = false;
    return openSource(t.path, t.start || 0);
  }
  conversationClick(e);
});

window.addEventListener('keydown', (e) => {
  const modalOpen = !$('#modal').hidden;
  if (modalOpen && (e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'f') {
    e.preventDefault();
    return focusFind();
  }
  if (walker && modalOpen && !e.ctrlKey && !e.metaKey && !e.altKey && !e.target.closest('input, textarea')) {
    if (e.key === ']' || e.key === 'j' || e.key === 'J') { e.preventDefault(); return showWalkStep(walker.index + 1); }
    if (e.key === '[' || e.key === 'k' || e.key === 'K') { e.preventDefault(); return showWalkStep(walker.index - 1); }
    if (e.key === ' ') { e.preventDefault(); return toggleWalkPlay(); }
  }
  if (e.key === 'Escape') {
    if (modalOpen && !$('#sym').hidden) { $('#sym').hidden = true; return; }
    if (modalOpen && walker) return exitWalk();
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
  const sel = state.selected;
  if ((e.key === 'h' || e.key === 'H') && !e.ctrlKey && !e.metaKey && !e.altKey && sel?.type === 'node' && state.map) {
    setHidden([sel.id], !isHidden(sel.id));
  }
});

// ---------- start ----------

(async () => {
  try {
    state.repo = await api('/api/repo');
    $('#repo').textContent = `${state.repo.root}${state.repo.commit ? ` @ ${state.repo.commit.slice(0, 10)}` : ''}`;
    $('#repo').title = $('#repo').textContent;
    document.querySelectorAll('[data-engine-name]').forEach((el) => { el.textContent = engineName('ask'); });
  } catch (err) {
    return showError(err.message, '');
  }
  load(scopeFromHash());
})();
