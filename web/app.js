import { decode as msgpackDecode } from 'https://cdn.jsdelivr.net/npm/@msgpack/msgpack@3/+esm';
import {
  interpolateTurbo, interpolateViridis, interpolatePlasma,
  interpolateInferno, interpolateMagma, interpolateCividis,
  interpolateSpectral, interpolateRainbow, interpolateCool, interpolateWarm,
  schemeTableau10, schemeCategory10, schemeSet2, schemeSet3,
  schemeDark2, schemePaired, schemeAccent,
} from 'https://cdn.jsdelivr.net/npm/d3-scale-chromatic@3/+esm';

const PERIOD_SCHEMES = {
  Turbo: interpolateTurbo,
  Viridis: interpolateViridis,
  Plasma: interpolatePlasma,
  Inferno: interpolateInferno,
  Magma: interpolateMagma,
  Cividis: interpolateCividis,
  Spectral: t => interpolateSpectral(1 - t), // reversed: dark=old, bright=new
  Rainbow: interpolateRainbow,
  Cool: interpolateCool,
  Warm: interpolateWarm,
};

const AUTHOR_SCHEMES = {
  Tableau10: schemeTableau10,
  Category10: schemeCategory10,
  Set2: schemeSet2,
  Set3: schemeSet3,
  Dark2: schemeDark2,
  Paired: schemePaired,
  Accent: schemeAccent,
};

let periodInterpolator = interpolateViridis;
let authorScheme = schemeTableau10;

const OTHER_COLOR = '#484f58';

// ── Theme ─────────────────────────────────────────────────────────────────────

const CANVAS_DARK = {
  grid:       '#21262d',
  axisBorder: '#30363d',
  label:      '#b1bac4',
  msg:        '#8b949e',
  hover:      'rgba(255,255,255,0.45)',
  bandEdge:   'rgba(0,0,0,0.3)',
  tagLine:    '139,148,158',
  tagText:    '230,237,243',
};

const CANVAS_LIGHT = {
  grid:       '#e8ecf0',
  axisBorder: '#d0d7de',
  label:      '#57606a',
  msg:        '#8c959f',
  hover:      'rgba(0,0,0,0.3)',
  bandEdge:   'rgba(0,0,0,0.12)',
  tagLine:    '90,100,115',
  tagText:    '20,30,45',
};

const darkMQ = window.matchMedia('(prefers-color-scheme: dark)');
let currentThemePref = 'auto';
let C = darkMQ.matches ? CANVAS_DARK : CANVAS_LIGHT;

function applyTheme(pref) {
  currentThemePref = pref;
  const resolved = pref === 'auto' ? (darkMQ.matches ? 'dark' : 'light') : pref;
  document.documentElement.setAttribute('data-theme', resolved);
  C = resolved === 'light' ? CANVAS_LIGHT : CANVAS_DARK;
  invalidateBands();
  if (data) scheduleRender();
}

darkMQ.addEventListener('change', () => {
  if (currentThemePref === 'auto') applyTheme('auto');
});

const canvas = document.getElementById('chart');
const ctx = canvas.getContext('2d');
const statusEl = document.getElementById('status');
const selectEl = document.getElementById('repo-select');
const metaEl = document.getElementById('meta');

let data = null;
let viewport = { xMin: 0, xMax: 1 };
let drag = null; // { startX, origXMin, origXMax } when dragging
let hoveredTs = null;
let viewMode = 'period'; // 'period' | 'author'
let authorColors = null; // Map<authorName, hexColor>, precomputed at loadRepo time

const tooltip = document.getElementById('tooltip');

// ── Render state ──────────────────────────────────────────────────────────────

let bandCanvas = null; // OffscreenCanvas for the expensive static layer
let bandCtx = null;
let bandsDirty = true; // set when viewport / data / viewMode changes

// Cached visible series at last band render
let visibleRaw = null;    // full-resolution filtered series (used by hover hit-test)
let visibleRender = null; // decimated to canvas pixels (used by drawBands)
let cachedStacks = null;  // cumulative period stacks for visibleRender
let cachedMaxTotal = 0;

// Cached margin computation (invalidated by canvas size or viewport change)
let marginCache = null; // { W, H, xMin, xMax, value }

// rAF deduplication — prevents multiple renders per animation frame
let renderPending = false;

function scheduleRender() {
  if (!renderPending) {
    renderPending = true;
    requestAnimationFrame(() => { renderPending = false; render(); });
  }
}

function invalidateBands() {
  bandsDirty = true;
}

function expandSparse(sparse, n) {
  const arr = new Float64Array(n);
  for (const [i, v] of sparse) arr[i] = v;
  return arr;
}

// ── Data loading ──────────────────────────────────────────────────────────────

async function init() {
  try {
    const resp = await fetch('../data/repos.json');
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const repos = await resp.json();

    selectEl.textContent = '';
    if (repos.length === 0) {
      const placeholder = document.createElement('option');
      placeholder.value = '';
      placeholder.textContent = 'Select a repository…';
      selectEl.appendChild(placeholder);
    }

    for (const repo of repos) {
      const opt = document.createElement('option');
      opt.value = repo;
      opt.textContent = repo;
      selectEl.appendChild(opt);
    }
    selectEl.disabled = false;

    const hashRepo = location.hash.replace('#', '').split('/')[0];
    const preselect = (hashRepo && repos.includes(hashRepo)) ? hashRepo : repos[0];
    if (preselect) {
      selectEl.value = preselect;
      await loadRepo(preselect);
    } else {
      statusEl.textContent = 'Select a repository to begin.';
    }
  } catch (e) {
    statusEl.textContent = `Could not load repos.json — run strata first. (${e.message})`;
  }
}

selectEl.addEventListener('change', () => {
  if (selectEl.value) loadRepo(selectEl.value);
});

async function loadRepo(name) {
  statusEl.textContent = 'Loading…';
  metaEl.textContent = '';
  try {
    const resp = await fetch(`../data/${name}.msgpack`);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    data = msgpackDecode(await resp.arrayBuffer());
    location.hash = name;

    if (!data.series || data.series.length === 0) {
      statusEl.textContent = 'No data in this file.';
      return;
    }

    // Expand sparse arrays to dense so all downstream code uses index access
    const nPeriods = data.periods.length;
    const nAuthors = data.authors ? data.authors.length : 0;
    for (const s of data.series) {
      s.counts = expandSparse(s.counts, nPeriods);
      s.author_counts = expandSparse(s.author_counts || [], nAuthors);
    }

    const tss = data.series.map(s => s.ts);
    let tMin = tss[0], tMax = tss[0];
    for (const t of tss) { if (t < tMin) tMin = t; if (t > tMax) tMax = t; }
    const pad = (tMax - tMin) * 0.03 || 3600 * 24 * 30;
    viewport = { xMin: tMin - pad, xMax: tMax + pad };

    // Precompute stable author→color mapping once per repo load
    authorColors = new Map();
    if (data.authors && data.authors.length > 0) {
      data.authors.forEach((author, i) => {
        if (author === 'other') {
          authorColors.set(author, OTHER_COLOR);
        } else {
          authorColors.set(author, authorScheme[i % authorScheme.length]);
        }
      });
    }

    const shortHash = data.head_commit ? data.head_commit.slice(0, 7) : '';
    metaEl.textContent =
      `${data.periods.length} periods · ${data.series.length} snapshots · ${data.granularity}`
      + (shortHash ? ` · ${shortHash}` : '');
    statusEl.textContent = '';
    invalidateBands();
    scheduleRender();
  } catch (e) {
    statusEl.textContent = `Failed to load ${name}: ${e.message}`;
  }
}

// ── Margin caching ────────────────────────────────────────────────────────────

function getMargin(W, H, dpr) {
  const { xMin, xMax } = viewport;
  if (marginCache
      && marginCache.W === W && marginCache.H === H
      && marginCache.xMin === xMin && marginCache.xMax === xMax) {
    return marginCache.value;
  }
  const value = computeMargins(dpr);
  marginCache = { W, H, xMin, xMax, value };
  return value;
}

function computeMargins(dpr) {
  ctx.save();

  // Left: widest y-axis tick label
  let maxTotal = 0;
  if (data) for (const s of data.series) if (s.total > maxTotal) maxTotal = s.total;
  ctx.font = `${11 * dpr}px monospace`;
  const yLabelW = Math.max(
    ...([0, 0.25, 0.5, 0.75, 1].map(f => ctx.measureText(formatNum(maxTotal * f)).width))
  );
  const left = yLabelW + 14 * dpr;

  // Top: vertical space needed for the tallest rotated tag label in view
  let top = 20 * dpr;
  if (data && data.tags && data.tags.length > 0) {
    const { xMin, xMax } = viewport;
    ctx.font = `bold ${11 * dpr}px monospace`;
    const visibleWidths = data.tags
      .filter(t => t.ts >= xMin && t.ts <= xMax)
      .map(t => ctx.measureText(t.name).width);
    if (visibleWidths.length > 0)
      top = Math.min(Math.max(...visibleWidths) * Math.SQRT1_2 + 16 * dpr, 64 * dpr);
  }

  // Bottom: date label height + gap
  ctx.font = `${11 * dpr}px monospace`;
  const bottom = 22 * dpr + 11 * dpr;

  // Right: half a date label so the rightmost tick doesn't clip
  const right = ctx.measureText('2000-01').width / 2 + 10 * dpr;

  ctx.restore();
  return { top, right, bottom, left };
}

function resizeCanvas() {
  const rect = canvas.parentElement.getBoundingClientRect();
  const dpr = devicePixelRatio || 1;
  const w = Math.round(rect.width * dpr);
  const h = Math.round(rect.height * dpr);
  if (canvas.width !== w || canvas.height !== h) {
    canvas.width = w;
    canvas.height = h;
    canvas.style.width = rect.width + 'px';
    canvas.style.height = rect.height + 'px';
    invalidateBands();
  }
}

// ── Series decimation ─────────────────────────────────────────────────────────

// Thin an array of series points to at most maxPts for rendering.
// Linear sampling preserves first and last points; result is visually lossless
// at ≤1 point per device pixel.
function decimateSeries(series, maxPts) {
  if (series.length <= maxPts) return series;
  const out = new Array(maxPts);
  for (let i = 0; i < maxPts; i++) {
    out[i] = series[Math.round(i * (series.length - 1) / (maxPts - 1))];
  }
  return out;
}

// ── Band drawing ──────────────────────────────────────────────────────────────

function drawPeriodBands(bctx, visible, stacks, nPeriods, xScale, yScale) {
  // Precompute colors once — interpolated continuously so every period
  // gets a unique, evenly-spaced hue regardless of how many there are.
  const colors = Array.from({ length: nPeriods }, (_, j) =>
    periodInterpolator(nPeriods <= 1 ? 0 : j / (nPeriods - 1))
  );

  for (let j = 0; j < nPeriods; j++) {
    bctx.beginPath();
    for (let i = 0; i < visible.length; i++) {
      const x = xScale(visible[i].ts);
      const y = yScale(stacks[i][j]);
      if (i === 0) bctx.moveTo(x, y);
      else bctx.lineTo(x, y);
    }
    for (let i = visible.length - 1; i >= 0; i--) {
      const x = xScale(visible[i].ts);
      const y = yScale(j > 0 ? stacks[i][j - 1] : 0);
      bctx.lineTo(x, y);
    }
    bctx.closePath();
    bctx.fillStyle = colors[j];
    bctx.fill();

    // Thin separator along the top edge — dark stroke works on all hues
    bctx.beginPath();
    for (let i = 0; i < visible.length; i++) {
      const x = xScale(visible[i].ts);
      const y = yScale(stacks[i][j]);
      if (i === 0) bctx.moveTo(x, y);
      else bctx.lineTo(x, y);
    }
    bctx.strokeStyle = C.bandEdge;
    bctx.lineWidth = 0.75;
    bctx.stroke();
  }
}

function drawAuthorBands(bctx, visible, xScale, yScale) {
  if (!data.authors || !data.authors.length) return;
  const nAuthors = data.authors.length;

  const authorStacks = visible.map(s => {
    const cum = new Float64Array(nAuthors);
    let acc = 0;
    for (let j = 0; j < nAuthors; j++) {
      acc += (s.author_counts && s.author_counts[j]) || 0;
      cum[j] = acc;
    }
    return cum;
  });

  for (let j = 0; j < nAuthors; j++) {
    bctx.fillStyle = authorColors.get(data.authors[j]) || OTHER_COLOR;
    bctx.beginPath();

    for (let i = 0; i < visible.length; i++) {
      const x = xScale(visible[i].ts);
      const y = yScale(authorStacks[i][j]);
      if (i === 0) bctx.moveTo(x, y);
      else bctx.lineTo(x, y);
    }

    for (let i = visible.length - 1; i >= 0; i--) {
      const x = xScale(visible[i].ts);
      const y = yScale(j > 0 ? authorStacks[i][j - 1] : 0);
      bctx.lineTo(x, y);
    }

    bctx.closePath();
    bctx.fill();
  }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

function render() {
  resizeCanvas();
  if (!data || !data.series.length) return;

  const dpr = devicePixelRatio || 1;
  const W = canvas.width;
  const H = canvas.height;

  // Recreate offscreen canvas if dimensions changed (resizeCanvas sets bandsDirty)
  if (!bandCanvas || bandCanvas.width !== W || bandCanvas.height !== H) {
    bandCanvas = new OffscreenCanvas(W, H);
    bandCtx = bandCanvas.getContext('2d');
    bandsDirty = true;
  }

  const margin = getMargin(W, H, dpr);
  const plotW = W - margin.left - margin.right;
  const plotH = H - margin.top - margin.bottom;

  const { xMin, xMax } = viewport;
  const xRange = Math.max(xMax - xMin, 1);
  const xScale = ts => (ts - xMin) / xRange * plotW + margin.left;

  if (bandsDirty) {
    const rawVisible = data.series.filter(
      s => s.ts >= xMin - xRange * 0.02 && s.ts <= xMax + xRange * 0.02
    );

    if (rawVisible.length < 2) {
      ctx.clearRect(0, 0, W, H);
      ctx.fillStyle = C.msg;
      ctx.font = `${13 * dpr}px monospace`;
      ctx.textAlign = 'center';
      ctx.fillText('Zoom out to see data', W / 2, H / 2);
      visibleRaw = rawVisible;
      visibleRender = rawVisible;
      cachedStacks = [];
      cachedMaxTotal = 0;
      bandsDirty = false;
      return;
    }

    visibleRaw = rawVisible;
    // Decimate to ≤1 point per device pixel — visually lossless, much cheaper to draw
    visibleRender = decimateSeries(rawVisible, Math.ceil(plotW) + 1);

    const nPeriods = data.periods.length;
    cachedStacks = visibleRender.map(s => {
      const cum = new Float64Array(nPeriods);
      let acc = 0;
      for (let j = 0; j < nPeriods; j++) { acc += s.counts[j] || 0; cum[j] = acc; }
      return cum;
    });
    cachedMaxTotal = 0;
    for (const c of cachedStacks) {
      const v = c[nPeriods - 1] || 0;
      if (v > cachedMaxTotal) cachedMaxTotal = v;
    }

    const bctx = bandCtx;
    bctx.clearRect(0, 0, W, H);

    if (cachedMaxTotal > 0) {
      const yScale = val => margin.top + plotH - (val / cachedMaxTotal) * plotH;

      if (viewMode === 'period') {
        drawPeriodBands(bctx, visibleRender, cachedStacks, nPeriods, xScale, yScale);
      } else {
        drawAuthorBands(bctx, visibleRender, xScale, yScale);
      }

      drawAxes(bctx, margin, plotW, plotH, xMin, xMax, xRange, cachedMaxTotal, xScale, yScale, dpr);

      if (data.tags && data.tags.length > 0) {
        drawTags(bctx, data.tags, viewport, xScale, margin, plotW, plotH, dpr);
      }
    }

    bandsDirty = false;
  }

  // Blit static layer, then draw the hover crosshair on top
  ctx.clearRect(0, 0, W, H);
  ctx.drawImage(bandCanvas, 0, 0);

  if (hoveredTs !== null) {
    const x = xScale(hoveredTs);
    if (x >= margin.left && x <= margin.left + plotW) {
      ctx.save();
      ctx.setLineDash([4 * dpr, 3 * dpr]);
      ctx.strokeStyle = C.hover;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(x, margin.top);
      ctx.lineTo(x, margin.top + plotH);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.restore();
    }
  }
}

function drawAxes(bctx, margin, plotW, plotH, xMin, xMax, xRange, maxTotal, xScale, yScale, dpr) {
  bctx.strokeStyle = C.grid;
  bctx.lineWidth = 1;

  // Y gridlines
  const yTicks = 5;
  bctx.fillStyle = C.label;
  bctx.font = `${11 * dpr}px monospace`;
  bctx.textAlign = 'right';
  for (let i = 0; i <= yTicks; i++) {
    const val = (maxTotal * i) / yTicks;
    const y = yScale(val);
    bctx.beginPath();
    bctx.moveTo(margin.left, y);
    bctx.lineTo(margin.left + plotW, y);
    bctx.stroke();
    bctx.fillText(formatNum(val), margin.left - 4 * dpr, y + 3.5 * dpr);
  }

  // X gridlines + labels
  const maxXTicks = Math.max(2, Math.floor(plotW / (80 * dpr)));
  bctx.textAlign = 'center';
  for (let i = 0; i <= maxXTicks; i++) {
    const ts = xMin + (i / maxXTicks) * xRange;
    const x = xScale(ts);
    bctx.beginPath();
    bctx.moveTo(x, margin.top);
    bctx.lineTo(x, margin.top + plotH);
    bctx.stroke();
    bctx.fillText(formatDate(ts), x, margin.top + plotH + 16 * dpr);
  }

  // Axis border
  bctx.strokeStyle = C.axisBorder;
  bctx.lineWidth = 1;
  bctx.beginPath();
  bctx.moveTo(margin.left, margin.top);
  bctx.lineTo(margin.left, margin.top + plotH);
  bctx.lineTo(margin.left + plotW, margin.top + plotH);
  bctx.stroke();
}

function drawTags(bctx, tags, viewport, xScale, margin, plotW, plotH, dpr) {
  const { xMin, xMax } = viewport;
  const visibleTags = tags.filter(t => t.ts >= xMin && t.ts <= xMax);
  if (visibleTags.length === 0) return;

  const plotWidthPx = xScale(xMax) - xScale(xMin);

  // Measure label widths upfront so we can do accurate overlap detection
  bctx.save();
  bctx.font = `${9 * dpr}px monospace`;
  const widths = new Map(visibleTags.map(t => [t, bctx.measureText(t.name).width]));
  bctx.restore();

  // Start with top candidates by importance, then sort by position
  const maxCandidates = Math.max(1, Math.floor(plotWidthPx / (20 * dpr)));
  let toDisplay = [...visibleTags]
    .sort((a, b) => b.importance - a.importance)
    .slice(0, maxCandidates)
    .sort((a, b) => a.ts - b.ts);

  // Iteratively drop the less-important member of any overlapping adjacent pair.
  // Labels are rotated -45°, so a label of pixel-width w occupies w*cos(45°) on the x-axis.
  let changed = true;
  while (changed) {
    changed = false;
    for (let i = 0; i < toDisplay.length - 1; i++) {
      const a = toDisplay[i], b = toDisplay[i + 1];
      const aLabelEnd = xScale(a.ts) + widths.get(a) * Math.SQRT1_2 + 6 * dpr;
      if (aLabelEnd > xScale(b.ts)) {
        toDisplay.splice(a.importance >= b.importance ? i + 1 : i, 1);
        changed = true;
        break;
      }
    }
  }

  for (const tag of toDisplay) {
    const x = xScale(tag.ts);
    const alpha = 0.25 + tag.importance * 0.75;

    bctx.setLineDash([3 * dpr, 3 * dpr]);
    bctx.strokeStyle = `rgba(${C.tagLine},${alpha})`;
    bctx.lineWidth = 1;
    bctx.beginPath();
    bctx.moveTo(x, margin.top);
    bctx.lineTo(x, margin.top + plotH);
    bctx.stroke();
    bctx.setLineDash([]);

    bctx.save();
    bctx.translate(x + 3 * dpr, margin.top);
    bctx.rotate(-Math.PI / 4);
    bctx.fillStyle = `rgba(${C.tagText},${0.5 + alpha * 0.5})`;
    bctx.font = `bold ${11 * dpr}px monospace`;
    bctx.textAlign = 'left';
    bctx.fillText(tag.name, 0, 0);
    bctx.restore();
  }
}

function formatNum(n) {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(0) + 'k';
  return Math.round(n).toString();
}

function formatDate(ts) {
  const d = new Date(ts * 1000);
  return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0');
}

// ── Zoom & pan ────────────────────────────────────────────────────────────────

canvas.addEventListener('wheel', e => {
  if (!data) return;
  e.preventDefault();
  hideTooltip();
  const rect = canvas.getBoundingClientRect();
  const frac = (e.clientX - rect.left) / rect.width;
  const { xMin, xMax } = viewport;
  const center = xMin + frac * (xMax - xMin);
  const factor = Math.exp(e.deltaY * 0.001);
  viewport = {
    xMin: center - (center - xMin) * factor,
    xMax: center + (xMax - center) * factor,
  };
  invalidateBands();
  scheduleRender();
}, { passive: false });

canvas.addEventListener('mousedown', e => {
  hideTooltip();
  drag = { startX: e.clientX, origXMin: viewport.xMin, origXMax: viewport.xMax };
  canvas.classList.add('dragging');
});

canvas.addEventListener('mousemove', e => {
  if (drag || !data) return;

  const dpr = devicePixelRatio || 1;
  const W = canvas.width;
  const H = canvas.height;
  const margin = getMargin(W, H, dpr);
  const plotW = W - margin.left - margin.right;
  const plotH = H - margin.top - margin.bottom;

  const rect = canvas.getBoundingClientRect();
  const canvasX = (e.clientX - rect.left) * dpr;
  if (canvasX < margin.left || canvasX > margin.left + plotW) {
    hideTooltip();
    return;
  }

  const { xMin, xMax } = viewport;
  const xRange = Math.max(xMax - xMin, 1);
  const ts = xMin + ((canvasX - margin.left) / plotW) * xRange;

  // Use full-resolution visible for accurate nearest-point detection
  const visible = visibleRaw;
  if (!visible || !visible.length) { hideTooltip(); return; }

  let nearest = visible[0];
  let minDist = Math.abs(visible[0].ts - ts);
  for (const s of visible) {
    const d = Math.abs(s.ts - ts);
    if (d < minDist) { minDist = d; nearest = s; }
  }

  if (nearest.ts !== hoveredTs) {
    hoveredTs = nearest.ts;
    scheduleRender();
  }

  const nPeriods = data.periods.length;
  const cum = new Float64Array(nPeriods);
  let acc = 0;
  for (let j = 0; j < nPeriods; j++) { acc += nearest.counts[j] || 0; cum[j] = acc; }
  const snapTotal = cum[nPeriods - 1];

  const canvasY = (e.clientY - rect.top) * dpr;

  // Determine which band the cursor falls in (period or author depending on view)
  let hoveredBand = null;
  if (cachedMaxTotal > 0 && canvasY >= margin.top && canvasY <= margin.top + plotH) {
    const val = (margin.top + plotH - canvasY) / plotH * cachedMaxTotal;

    if (viewMode === 'period') {
      for (let j = 0; j < nPeriods; j++) {
        if (val <= cum[j]) {
          const pct = snapTotal > 0 ? (nearest.counts[j] / snapTotal * 100).toFixed(1) : '0';
          hoveredBand = { label: 'period', name: data.periods[j], pct };
          break;
        }
      }
    } else if (data.authors && nearest.author_counts) {
      const nAuthors = data.authors.length;
      const authorCum = new Float64Array(nAuthors);
      let accA = 0;
      for (let j = 0; j < nAuthors; j++) {
        accA += nearest.author_counts[j] || 0;
        authorCum[j] = accA;
      }
      for (let j = 0; j < nAuthors; j++) {
        if (val <= authorCum[j]) {
          const pct = snapTotal > 0 ? ((nearest.author_counts[j] || 0) / snapTotal * 100).toFixed(1) : '0';
          hoveredBand = { label: 'author', name: data.authors[j], pct };
          break;
        }
      }
    }
  }

  const fullIdx = data.series.findIndex(s => s.ts === nearest.ts);
  const total = nearest.total;
  const prev = fullIdx > 0 ? data.series[fullIdx - 1].total : null;
  const delta = prev !== null ? total - prev : null;

  showTooltip(e.clientX, e.clientY, nearest, total, delta, hoveredBand);
});

canvas.addEventListener('mouseleave', () => {
  hideTooltip();
});

function makeRow(label, value) {
  const row = document.createElement('div');
  row.className = 'tip-row';
  const lbl = document.createElement('span');
  lbl.className = 'tip-label';
  lbl.textContent = label;
  const val = document.createElement('span');
  val.textContent = value;
  row.append(lbl, val);
  return row;
}

function positionTooltip(cx, cy) {
  const pad = 14;
  let left = cx + pad;
  let top = cy - tooltip.offsetHeight - pad;
  if (left + tooltip.offsetWidth > window.innerWidth - 8) left = cx - tooltip.offsetWidth - pad;
  if (top < 8) top = cy + pad;
  tooltip.style.left = left + 'px';
  tooltip.style.top = top + 'px';
}

function showTooltip(cx, cy, snap, total, delta, hoveredBand) {
  const date = new Date(snap.ts * 1000);
  const dateStr = date.getFullYear() + '-'
    + String(date.getMonth() + 1).padStart(2, '0') + '-'
    + String(date.getDate()).padStart(2, '0');

  tooltip.replaceChildren();

  if (viewMode === 'author') {
    if (hoveredBand) {
      const name = document.createElement('div');
      name.className = 'tip-summary';
      name.textContent = hoveredBand.name;
      tooltip.appendChild(name);
      const authorLines = hoveredBand.pct !== undefined
        ? (snap.author_counts && data.authors
            ? formatNum(snap.author_counts[data.authors.indexOf(hoveredBand.name)] || 0)
            : formatNum(total))
        : formatNum(total);
      tooltip.appendChild(makeRow('lines', authorLines));
      tooltip.appendChild(makeRow('share', `${hoveredBand.pct}%`));
    } else {
      tooltip.appendChild(makeRow('lines', formatNum(total)));
    }
    tooltip.style.display = 'block';
    positionTooltip(cx, cy);
    return;
  }

  const summary = document.createElement('div');
  summary.className = 'tip-summary';
  summary.textContent = snap.summary || '(no message)';
  tooltip.appendChild(summary);

  tooltip.appendChild(makeRow('by', snap.author || 'unknown'));
  tooltip.appendChild(makeRow('date', dateStr));

  if (hoveredBand) {
    tooltip.appendChild(makeRow(hoveredBand.label, `${hoveredBand.name} · ${hoveredBand.pct}%`));
  }

  const linesRow = makeRow('lines', formatNum(total));
  if (delta !== null) {
    const badge = document.createElement('span');
    badge.className = 'tip-delta ' + (delta >= 0 ? 'pos' : 'neg');
    badge.textContent = (delta >= 0 ? '+' : '\u2212') + formatNum(Math.abs(delta));
    linesRow.lastChild.append('\u00a0', badge);
  }
  tooltip.appendChild(linesRow);

  tooltip.style.display = 'block';
  positionTooltip(cx, cy);
}

function hideTooltip() {
  tooltip.style.display = 'none';
  if (hoveredTs !== null) {
    hoveredTs = null;
    if (data) scheduleRender();
  }
}

window.addEventListener('mousemove', e => {
  if (!drag || !data) return;
  const rect = canvas.getBoundingClientRect();
  const dxFrac = (e.clientX - drag.startX) / rect.width;
  const range = drag.origXMax - drag.origXMin;
  viewport = {
    xMin: drag.origXMin - dxFrac * range,
    xMax: drag.origXMax - dxFrac * range,
  };
  invalidateBands();
  scheduleRender();
});

window.addEventListener('mouseup', () => {
  drag = null;
  canvas.classList.remove('dragging');
});

window.addEventListener('resize', () => {
  if (data) {
    invalidateBands();
    scheduleRender();
  }
});

// ── View switcher ─────────────────────────────────────────────────────────────

document.querySelectorAll('.view-btn').forEach(btn => {
  btn.addEventListener('click', () => {
    viewMode = btn.dataset.view;
    document.querySelectorAll('.view-btn').forEach(b => b.classList.toggle('active', b === btn));
    invalidateBands();
    scheduleRender();
  });
});

// ── Settings panel ────────────────────────────────────────────────────────────

const settingsBtn = document.getElementById('settings-btn');
const settingsPanel = document.getElementById('settings-panel');

settingsBtn.addEventListener('click', e => {
  e.stopPropagation();
  const open = settingsPanel.hidden;
  settingsPanel.hidden = !open;
  settingsBtn.classList.toggle('open', open);
});

document.addEventListener('click', () => {
  if (!settingsPanel.hidden) {
    settingsPanel.hidden = true;
    settingsBtn.classList.remove('open');
  }
});

settingsPanel.addEventListener('click', e => e.stopPropagation());

document.querySelectorAll('[data-theme-btn]').forEach(btn => {
  btn.addEventListener('click', () => {
    document.querySelectorAll('[data-theme-btn]').forEach(b => b.classList.toggle('active', b === btn));
    applyTheme(btn.dataset.themeBtn);
  });
});

document.getElementById('period-scheme-select').addEventListener('change', e => {
  periodInterpolator = PERIOD_SCHEMES[e.target.value];
  invalidateBands();
  scheduleRender();
});

document.getElementById('author-scheme-select').addEventListener('change', e => {
  authorScheme = AUTHOR_SCHEMES[e.target.value];
  // Recompute author→color mapping with the new scheme
  if (data && data.authors) {
    authorColors = new Map();
    data.authors.forEach((author, i) => {
      if (author === 'other') {
        authorColors.set(author, OTHER_COLOR);
      } else {
        authorColors.set(author, authorScheme[i % authorScheme.length]);
      }
    });
  }
  invalidateBands();
  scheduleRender();
});

init();
