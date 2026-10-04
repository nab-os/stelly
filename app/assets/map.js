// Canvas map for the suggestion space.
//
// Owns rendering and all pan/zoom/hover, so dragging never crosses into Rust.
// Points arrive once as binary; only selections go back.
//
// The shape of this file is dictated by one measurement. In WebKitGTK, which
// is what the desktop webview is, filling 28k small paths costs ~300ms a frame
// and batching them into one path per colour costs ~1100ms, Cairo tessellates
// a 28k-subpath far worse than it rasterises 28k small ones. Canvas path APIs
// simply cannot draw this many points at interactive rates.
//
// So the cloud is not drawn per frame. It is rasterised once, by hand, into an
// offscreen buffer covering the viewport plus a margin, and every frame after
// that is a blit of that buffer: ~2ms to pan, ~9ms to zoom. The buffer is
// rebuilt only when the gesture settles at a new scale or the pan runs off its
// edge. Overlays, route, markers, labels, are a handful of shapes and stay
// on the normal canvas API, drawn on top each frame.

(async () => {
  const canvas = document.getElementById("map");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");

  const PALETTE = [
    "#7aa2f7", "#9ece6a", "#e0af68", "#f7768e", "#bb9af7",
    "#7dcfff", "#ff9e64", "#73daca", "#c0caf5", "#b4f9f8",
  ];
  const RGB = PALETTE.map((h) => [
    parseInt(h.slice(1, 3), 16),
    parseInt(h.slice(3, 5), 16),
    parseInt(h.slice(5, 7), 16),
  ]);

  const TAU = Math.PI * 2;

  // Reloadable: hiding an artist removes their points, and the payload is
  // rebuilt server-side, so the views have to be rebound rather than patched.
  let meta, n, ids, xy, genre;
  // Track id -> point index. Rebuilt with the points, because the indices move.
  let byId = new Map();

  // The offscreen rasterisation of every visible point, in device pixels;
  // filled in by the cloud layer below. Declared up here because the initial
  // `loadPoints` invalidates it before that section is reached.
  const cloud = {
    canvas: null, ctx: null, img: null, bgRow: null,
    w: 0, h: 0, offX: 0, offY: 0,
    scale: 0, dpr: 0, dim: false, valid: false,
  };

  // Groups standing in for dense patches of points, rebuilt with the cloud;
  // see the cloud layer. `hidden[i]` is set for every point inside one, so
  // hover and click reach the group rather than a point that is not drawn.
  let clusters = [];
  let hidden = new Uint8Array(0);
  // Scratch for bucketing the visible points by bin, sized to the corpus.
  let visible = new Int32Array(0);
  let visibleBin = new Int32Array(0);
  let binOrder = new Int32Array(0);
  let tally = new Int32Array(0);
  // A fixed pseudo-random rank per point, from its track id, deciding which
  // stars a crowded patch keeps. Fixed so the sky does not twinkle between
  // rebuilds, and so zooming in only ever adds stars to those already shown.
  let rank = new Float32Array(0);

  function rankOf(id) {
    let h = Math.imul((id % 4294967296) | 0, 0x9e3779b1) ^ ((id / 4294967296) | 0);
    h = Math.imul(h ^ (h >>> 16), 0x85ebca6b);
    h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
    return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
  }

  async function loadPoints() {
    meta = await (await fetch("/points/meta")).json();
    const buffer = await (await fetch("/points/data")).arrayBuffer();
    n = meta.n;
    // Views match the layout documented in map.rs.
    ids = new Float64Array(buffer, 0, n);
    xy = new Float32Array(buffer, 8 * n, 2 * n);
    genre = new Uint16Array(buffer, 16 * n, n);
    byId = new Map();
    for (let i = 0; i < n; i++) byId.set(ids[i], i);
    clusters = [];
    hidden = new Uint8Array(n);
    visible = new Int32Array(n);
    visibleBin = new Int32Array(n);
    binOrder = new Int32Array(n);
    tally = new Int32Array(Math.max(1, meta.genres.length));
    rank = new Float32Array(n);
    for (let i = 0; i < n; i++) rank[i] = rankOf(ids[i]);
    buildGrid();
    cloud.valid = false;
  }

  // ------------------------------------------------------------------- grid
  //
  // A uniform grid over layout space, built once per point load. Both the
  // hot loops need the same thing, the points inside a rectangle, and at
  // any zoom past "fit" that is a small fraction of the corpus.

  let grid = null;

  function buildGrid() {
    if (n === 0) {
      grid = null;
      return;
    }
    const b = meta.bounds;
    const spanX = Math.max(b.max_x - b.min_x, 1e-6);
    const spanY = Math.max(b.max_y - b.min_y, 1e-6);
    // ~2 points per cell on average: enough that a hover test touches tens of
    // points, few enough that the cell table stays small.
    const side = Math.max(1, Math.min(512, Math.round(Math.sqrt(n / 2))));
    // A hair wider than the data, so max_x lands inside the last cell.
    const cellW = (spanX * 1.0001) / side;
    const cellH = (spanY * 1.0001) / side;

    const cellOf = new Int32Array(n);
    const counts = new Int32Array(side * side + 1);
    for (let i = 0; i < n; i++) {
      let cx = ((xy[i * 2] - b.min_x) / cellW) | 0;
      let cy = ((xy[i * 2 + 1] - b.min_y) / cellH) | 0;
      if (cx < 0) cx = 0; else if (cx >= side) cx = side - 1;
      if (cy < 0) cy = 0; else if (cy >= side) cy = side - 1;
      const cell = cy * side + cx;
      cellOf[i] = cell;
      counts[cell + 1]++;
    }
    for (let c = 0; c < side * side; c++) counts[c + 1] += counts[c];

    // Counting sort: `order` lists point indices grouped by cell, and
    // `start[c]..start[c + 1]` is the slice belonging to cell c.
    const cursor = counts.slice(0, side * side);
    const order = new Int32Array(n);
    for (let i = 0; i < n; i++) order[cursor[cellOf[i]]++] = i;

    grid = { side, minX: b.min_x, minY: b.min_y, cellW, cellH, start: counts, order };
  }

  /// Clamp a layout coordinate to a column/row index.
  function col(x) {
    const c = ((x - grid.minX) / grid.cellW) | 0;
    return c < 0 ? 0 : c >= grid.side ? grid.side - 1 : c;
  }
  function row(y) {
    const c = ((y - grid.minY) / grid.cellH) | 0;
    return c < 0 ? 0 : c >= grid.side ? grid.side - 1 : c;
  }

  await loadPoints();

  let view = { scale: 1, offsetX: 0, offsetY: 0 };
  // Whether `view` has been fitted to real points yet. Not just to a real
  // rect: until the server answers, the points are `EMPTY_META`, whose bounds
  // are all zero, and fitting those zooms in by a factor of ~1e8.
  let fitted = false;
  let hover = -1;
  let hoverCluster = -1;
  // Where the mouse last was over the canvas, so hover can be re-resolved
  // when the groups under it are rebuilt. Null on touch, which has no hover.
  let pointer = null;
  let selected = -1;
  let route = [];

  // The canvas rect, cached. Reading it inside draw forces a layout on every
  // frame of a drag, which is exactly when there is no budget for one.
  let width = 0;
  let height = 0;

  const ratio = () => window.devicePixelRatio || 1;

  function measure() {
    const rect = canvas.getBoundingClientRect();
    width = rect.width;
    height = rect.height;
    return rect;
  }

  function resize() {
    const dpr = ratio();
    const rect = measure();
    canvas.width = rect.width * dpr;
    canvas.height = rect.height * dpr;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    cloud.valid = false;
    draw();
  }

  /// Fit the whole cloud to the canvas. False, leaving the view alone, when
  /// there is nothing to fit yet: no points or no rect.
  function fit() {
    const rect = measure();
    if (n === 0 || rect.width <= 0 || rect.height <= 0) return false;
    const spanX = Math.max(meta.bounds.max_x - meta.bounds.min_x, 1e-6);
    const spanY = Math.max(meta.bounds.max_y - meta.bounds.min_y, 1e-6);
    const pad = 30;
    view.scale = Math.min((rect.width - pad * 2) / spanX, (rect.height - pad * 2) / spanY);
    view.offsetX = pad - meta.bounds.min_x * view.scale;
    view.offsetY = pad - meta.bounds.min_y * view.scale;
    cloud.valid = false;
    return true;
  }

  const screenX = (i) => xy[i * 2] * view.scale + view.offsetX;
  const screenY = (i) => xy[i * 2 + 1] * view.scale + view.offsetY;
  const toScreen = (i) => [screenX(i), screenY(i)];

  const pointRadius = (scale) => Math.max(1.5, Math.min(4, scale * 0.08));

  // Grouping, kept a night sky. The screen is cut into BIN-pixel bins, and a
  // bin crowded enough that its stars would cover COVERAGE of it is thinned
  // to about that many stars, with a soft glow in its main genre's colour
  // standing in for the rest, a nebula rather than a smear. Zooming in
  // shrinks the bins in layout space and grows the stars, so the thinning
  // eases off and the hidden stars come back on their own.
  const BIN = 56;
  const COVERAGE = 0.25;

  /// How many points make a bin a group at this scale. Measured in covered
  /// area rather than a flat count: twenty dots of 1.5px are a speck, twenty
  /// of 4px are already a crowd. Never fewer than a handful, a group of two
  /// hides more than it tidies.
  const clusterMin = (scale) => {
    const r = pointRadius(scale);
    return Math.max(8, Math.round((BIN * BIN * COVERAGE) / (Math.PI * r * r)));
  };

  /// How far a group's glow reaches, in CSS pixels. Wider than a bin, so
  /// neighbouring glows run together into one cloud instead of a grid of
  /// spots, and wider still the more crowded it is.
  const glowRadius = (crowding) => Math.min(BIN * 1.3, BIN * 0.75 * (1 + 0.2 * Math.log2(crowding)));

  /// How bright a group's glow is at its centre. Faint on purpose: the stars
  /// are the picture, the glow only says that there are more of them here.
  const glowAlpha = (crowding) => Math.min(0.18, 0.05 + 0.04 * Math.log2(crowding));

  const formatCount = (count) =>
    count >= 10000 ? `${Math.round(count / 1000)}k`
      : count >= 1000 ? `${(count / 1000).toFixed(1)}k`
        : `${count}`;

  /// Paint the groups' glows over the rasterised stars, into the cloud
  /// buffer. Added rather than blended, so they light the sky up the way a
  /// nebula would and never cover a star. A few hundred gradients, once per
  /// rebuild rather than per frame, which the canvas API manages fine; it is
  /// the tens of thousands of stars it cannot.
  function drawClusters(c, S, offX, offY, dpr, dim) {
    if (clusters.length === 0) return;
    c.save();
    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = "lighter";
    for (const cluster of clusters) {
      const x = cluster.x * S + offX;
      const y = cluster.y * S + offY;
      const r = cluster.glow * dpr;
      const [cr, cg, cb] = RGB[cluster.genre % PALETTE.length];
      const a = dim ? cluster.alpha * 0.25 : cluster.alpha;
      const gradient = c.createRadialGradient(x, y, 0, x, y, r);
      gradient.addColorStop(0, `rgba(${cr},${cg},${cb},${a})`);
      gradient.addColorStop(0.45, `rgba(${cr},${cg},${cb},${a * 0.4})`);
      gradient.addColorStop(1, `rgba(${cr},${cg},${cb},0)`);
      c.fillStyle = gradient;
      c.fillRect(x - r, y - r, r * 2, r * 2);
    }
    c.restore();
  }

  /// The group under a screen position, if any. The buffer may be blitted at
  /// another scale mid-gesture, so the drawn radius is scaled to match.
  function clusterAt(px, py, slop = 3) {
    if (clusters.length === 0 || !cloud.scale) return -1;
    const k = view.scale / cloud.scale;
    let best = -1;
    let bestDistance = Infinity;
    for (let c = 0; c < clusters.length; c++) {
      const cluster = clusters[c];
      const dx = cluster.x * view.scale + view.offsetX - px;
      const dy = cluster.y * view.scale + view.offsetY - py;
      const reach = cluster.glow * 0.45 * k + slop;
      const d = dx * dx + dy * dy;
      if (d <= reach * reach && d < bestDistance) {
        bestDistance = d;
        best = c;
      }
    }
    return best;
  }

  /// Fly to a group, framing the points it holds. At least doubling the zoom,
  /// so a click always makes progress even on a tight knot, and at most 4x,
  /// so a click opens the group up rather than losing everything around it.
  let flight = 0;
  function zoomTo(cluster) {
    if (width <= 0 || height <= 0) return;
    const pad = 60;
    const spanX = Math.max(cluster.maxX - cluster.minX, 1e-6);
    const spanY = Math.max(cluster.maxY - cluster.minY, 1e-6);
    const fitScale = Math.min((width - pad * 2) / spanX, (height - pad * 2) / spanY);
    const from = {
      scale: view.scale,
      x: (width / 2 - view.offsetX) / view.scale,
      y: (height / 2 - view.offsetY) / view.scale,
    };
    const to = {
      scale: Math.min(Math.max(fitScale, view.scale * 2), view.scale * 4),
      x: (cluster.minX + cluster.maxX) / 2,
      y: (cluster.minY + cluster.maxY) / 2,
    };

    // Scale eases in log space, so the zoom feels even from start to end.
    const began = performance.now();
    const duration = 280;
    const id = ++flight;
    const step = (now) => {
      if (id !== flight) return;
      const t = Math.min(1, (now - began) / duration);
      const e = 1 - Math.pow(1 - t, 3);
      const scale = Math.exp(Math.log(from.scale) + (Math.log(to.scale) - Math.log(from.scale)) * e);
      const cx = from.x + (to.x - from.x) * e;
      const cy = from.y + (to.y - from.y) * e;
      view.scale = scale;
      view.offsetX = width / 2 - cx * scale;
      view.offsetY = height / 2 - cy * scale;
      draw();
      if (t < 1) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  }

  // A pan or a wheel tick can fire several times between two frames. Painting
  // each one is wasted work that arrives on screen as lag, so collapse them.
  let pending = false;
  function schedule() {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => {
      pending = false;
      draw();
    });
  }

  // ------------------------------------------------------------- cloud layer
  //
  // `cloud` itself is declared at the top of the file. `offX`/`offY` put a
  // layout coordinate into it: px = x * scale * dpr + offX.

  // The panel colour behind the canvas. The buffer is opaque, which turns
  // alpha compositing into a plain lerp with no per-pixel division.
  let background = null;
  function readBackground() {
    const host = canvas.parentElement || canvas;
    const parsed = (getComputedStyle(host).backgroundColor || "").match(/[\d.]+/g);
    background = parsed && parsed.length >= 3
      ? [+parsed[0], +parsed[1], +parsed[2]]
      : [26, 27, 38];
  }

  // A circular coverage kernel, supersampled for a soft edge and premultiplied
  // by the layer's alpha. Rebuilt only when the radius or the dimming changes.
  let stamp = null;
  function stampFor(radius, alpha) {
    if (stamp && stamp.radius === radius && stamp.alpha === alpha) return stamp;
    const side = Math.ceil(radius * 2) + 2;
    const half = side / 2;
    const cov = new Uint8Array(side * side);
    const rowFrom = new Int32Array(side);
    const rowTo = new Int32Array(side);
    for (let y = 0; y < side; y++) {
      let from = -1, to = 0;
      for (let x = 0; x < side; x++) {
        let hits = 0;
        for (let sy = 0; sy < 3; sy++) {
          for (let sx = 0; sx < 3; sx++) {
            const dx = x + (sx + 0.5) / 3 - half;
            const dy = y + (sy + 0.5) / 3 - half;
            if (dx * dx + dy * dy <= radius * radius) hits++;
          }
        }
        const a = Math.round((hits / 9) * alpha * 255);
        cov[y * side + x] = a;
        if (a > 0) {
          if (from < 0) from = x;
          to = x + 1;
        }
      }
      rowFrom[y] = from < 0 ? 0 : from;
      rowTo[y] = from < 0 ? 0 : to;
    }
    stamp = { radius, alpha, side, cov, rowFrom, rowTo, off: Math.floor(side / 2) };
    return stamp;
  }

  /// Rasterise every point that falls inside the buffer, by hand.
  function renderCloud() {
    const dpr = ratio();
    const vw = Math.max(1, Math.round(width * dpr));
    const vh = Math.max(1, Math.round(height * dpr));

    // Half a viewport of margin each way, so an ordinary drag never runs off
    // the buffer. Trimmed if that would make it an absurd amount of memory.
    let mx = 0, my = 0;
    for (const f of [0.5, 0.35, 0.2, 0.1, 0]) {
      mx = Math.round(vw * f);
      my = Math.round(vh * f);
      if ((vw + 2 * mx) * (vh + 2 * my) <= 4.5e6) break;
    }
    const cw = vw + 2 * mx;
    const ch = vh + 2 * my;

    if (!cloud.canvas) cloud.canvas = document.createElement("canvas");
    if (cloud.w !== cw || cloud.h !== ch) {
      cloud.canvas.width = cw;
      cloud.canvas.height = ch;
      cloud.ctx = cloud.canvas.getContext("2d");
      cloud.img = cloud.ctx.createImageData(cw, ch);
      cloud.bgRow = null;
      cloud.w = cw;
      cloud.h = ch;
    }
    if (!background) readBackground();
    if (!cloud.bgRow) {
      cloud.bgRow = new Uint8ClampedArray(cw * 4);
      for (let o = 0; o < cloud.bgRow.length; o += 4) {
        cloud.bgRow[o] = background[0];
        cloud.bgRow[o + 1] = background[1];
        cloud.bgRow[o + 2] = background[2];
        cloud.bgRow[o + 3] = 255;
      }
    }

    const d = cloud.img.data;
    const rowBytes = cw * 4;
    for (let y = 0; y < ch; y++) d.set(cloud.bgRow, y * rowBytes);

    const dim = route.length > 0;
    const S = view.scale * dpr;
    const offX = view.offsetX * dpr + mx;
    const offY = view.offsetY * dpr + my;

    clusters = [];
    hoverCluster = -1;
    hidden.fill(0);

    if (grid && n > 0) {
      const { side: gside, start, order } = grid;
      const k = stampFor(pointRadius(view.scale) * dpr, dim ? 0.18 : 0.8);
      const { side, cov, rowFrom, rowTo, off } = k;

      // Layout-space extent of the buffer, widened by one stamp so a point
      // just outside still contributes the half of itself that shows.
      const pad = side;
      const lx0 = (-pad - offX) / S;
      const lx1 = (cw + pad - offX) / S;
      const ly0 = (-pad - offY) / S;
      const ly1 = (ch + pad - offY) / S;
      const cx0 = col(Math.min(lx0, lx1));
      const cx1 = col(Math.max(lx0, lx1));
      const cy0 = row(Math.min(ly0, ly1));
      const cy1 = row(Math.max(ly0, ly1));

      // Bins are BIN screen pixels wide but numbered from the layout origin,
      // floor(x * S / B), so a pan at the same scale lands every point in the
      // same bin and the groups hold still under the finger.
      const B = BIN * dpr;
      const bx0 = Math.floor(Math.min(lx0, lx1) * S / B);
      const by0 = Math.floor(Math.min(ly0, ly1) * S / B);
      const nbx = Math.floor(Math.max(lx0, lx1) * S / B) - bx0 + 1;
      const nby = Math.floor(Math.max(ly0, ly1) * S / B) - by0 + 1;
      const bins = nbx * nby;
      const least = clusterMin(view.scale);
      const binStart = new Int32Array(bins + 1);

      // Pass one: which points show at all, and how many land in each bin.
      let m = 0;
      for (let cy = cy0; cy <= cy1; cy++) {
        const base = cy * gside;
        // Cells in a row are contiguous in `order`, so one slice covers the
        // whole span rather than one lookup per cell.
        const from = start[base + cx0];
        const to = start[base + cx1 + 1];
        for (let q = from; q < to; q++) {
          const i = order[q];
          const px = xy[i * 2] * S + offX;
          const py = xy[i * 2 + 1] * S + offY;
          if (px < -pad || py < -pad || px > cw + pad || py > ch + pad) continue;
          let bx = Math.floor(xy[i * 2] * S / B) - bx0;
          let by = Math.floor(xy[i * 2 + 1] * S / B) - by0;
          if (bx < 0) bx = 0; else if (bx >= nbx) bx = nbx - 1;
          if (by < 0) by = 0; else if (by >= nby) by = nby - 1;
          const b = by * nbx + bx;
          visible[m] = i;
          visibleBin[m] = b;
          m++;
          binStart[b + 1]++;
        }
      }
      for (let b = 0; b < bins; b++) binStart[b + 1] += binStart[b];
      const cursor = binStart.slice(0, bins);
      for (let q = 0; q < m; q++) binOrder[cursor[visibleBin[q]]++] = visible[q];

      const stampPoint = (i) => {
        const x0 = Math.round(xy[i * 2] * S + offX) - off;
        const y0 = Math.round(xy[i * 2 + 1] * S + offY) - off;
        if (x0 + side <= 0 || y0 + side <= 0 || x0 >= cw || y0 >= ch) return;

        const c = RGB[genre[i] % PALETTE.length];
        const cr = c[0], cg = c[1], cb = c[2];
        const yA = y0 < 0 ? -y0 : 0;
        const yB = y0 + side > ch ? ch - y0 : side;
        for (let yy = yA; yy < yB; yy++) {
          let a0 = rowFrom[yy], a1 = rowTo[yy];
          if (x0 + a0 < 0) a0 = -x0;
          if (x0 + a1 > cw) a1 = cw - x0;
          if (a0 >= a1) continue;
          const crow = yy * side;
          let o = ((y0 + yy) * cw + x0 + a0) * 4;
          for (let xx = a0; xx < a1; xx++, o += 4) {
            const a = cov[crow + xx];
            d[o] += ((cr - d[o]) * a) >> 8;
            d[o + 1] += ((cg - d[o + 1]) * a) >> 8;
            d[o + 2] += ((cb - d[o + 2]) * a) >> 8;
          }
        }
      };

      // How crowded each bin is, blurred over its neighbours. Thinning on the
      // raw count makes every bin edge a visible step in star density; on the
      // blurred one the sky thins out smoothly towards a crowded core.
      const crowd = new Float32Array(bins);
      for (let by = 0; by < nby; by++) {
        for (let bx = 0; bx < nbx; bx++) {
          let sum = 0, weight = 0;
          for (let dy = -1; dy <= 1; dy++) {
            const y = by + dy;
            if (y < 0 || y >= nby) continue;
            for (let dx = -1; dx <= 1; dx++) {
              const x = bx + dx;
              if (x < 0 || x >= nbx) continue;
              const w = (dx === 0 ? 2 : 1) * (dy === 0 ? 2 : 1);
              const b = y * nbx + x;
              sum += (binStart[b + 1] - binStart[b]) * w;
              weight += w;
            }
          }
          crowd[by * nbx + bx] = sum / weight;
        }
      }

      // Pass two: a sparse bin draws its stars as they are; a crowded one is
      // thinned to about `least` of them and becomes a group, whose glow
      // stands in for the rest.
      for (let b = 0; b < bins; b++) {
        const from = binStart[b];
        const to = binStart[b + 1];
        const count = to - from;
        if (count === 0) continue;
        const crowding = crowd[b] / least;
        if (crowding < 1 || count < least) {
          for (let q = from; q < to; q++) stampPoint(binOrder[q]);
          continue;
        }

        const keep = 1 / crowding;
        let sx = 0, sy = 0;
        let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
        let lead = 0;
        for (let q = from; q < to; q++) {
          const i = binOrder[q];
          const x = xy[i * 2];
          const y = xy[i * 2 + 1];
          sx += x;
          sy += y;
          if (x < minX) minX = x;
          if (x > maxX) maxX = x;
          if (y < minY) minY = y;
          if (y > maxY) maxY = y;
          const g = genre[i];
          if (++tally[g] > tally[lead]) lead = g;
          if (rank[i] < keep) stampPoint(i);
          else hidden[i] = 1;
        }
        const share = tally[lead] / count;
        for (let q = from; q < to; q++) tally[genre[binOrder[q]]] = 0;

        clusters.push({
          x: sx / count,
          y: sy / count,
          count,
          genre: lead,
          share,
          glow: glowRadius(crowding),
          alpha: glowAlpha(crowding),
          minX, maxX, minY, maxY,
        });
      }
    }

    cloud.ctx.putImageData(cloud.img, 0, 0);
    drawClusters(cloud.ctx, S, offX, offY, dpr, dim);
    cloud.offX = offX;
    cloud.offY = offY;
    cloud.scale = view.scale;
    cloud.dpr = dpr;
    cloud.dim = dim;
    cloud.valid = true;
    builtAt = performance.now();

    // The groups were just rebuilt, so whatever the mouse was over may now be
    // a different group, or the points that group came apart into.
    if (pointer) {
      const at = pick(pointer.x, pointer.y);
      hover = at.point;
      hoverCluster = at.group;
    }
  }

  /// Whether the buffer still reaches every corner of the viewport. When it
  /// does not, part of the screen has no points to show at all, which is worth
  /// a rebuild mid-gesture; merely being the wrong scale is not.
  function cloudCovers() {
    if (!cloud.valid) return false;
    const dpr = ratio();
    if (cloud.dpr !== dpr) return false;
    const k = view.scale / cloud.scale;
    const dx = view.offsetX * dpr - cloud.offX * k;
    const dy = view.offsetY * dpr - cloud.offY * k;
    return dx <= 0 && dy <= 0 &&
      dx + cloud.w * k >= width * dpr &&
      dy + cloud.h * k >= height * dpr;
  }

  /// True when the buffer no longer describes what should be on screen.
  function cloudStale() {
    if (!cloud.valid) return true;
    if (cloud.dpr !== ratio()) return true;
    if (cloud.dim !== (route.length > 0)) return true;
    if (cloud.scale !== view.scale) return true;
    return !cloudCovers();
  }

  /// Put the buffer on screen. A pure pan is a 1:1 crop; mid-zoom it is a
  /// scaled blit of a buffer built for a different scale, which is soft for
  /// the length of the gesture and sharp again the moment it settles.
  function blitCloud() {
    const dpr = cloud.dpr;
    ctx.save();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);

    if (cloud.scale === view.scale && dpr === ratio()) {
      let sx = Math.round(cloud.offX - view.offsetX * dpr);
      let sy = Math.round(cloud.offY - view.offsetY * dpr);
      let sw = Math.round(width * dpr);
      let sh = Math.round(height * dpr);
      let dx = 0, dy = 0;
      if (sx < 0) { dx = -sx; sw += sx; sx = 0; }
      if (sy < 0) { dy = -sy; sh += sy; sy = 0; }
      if (sx + sw > cloud.w) sw = cloud.w - sx;
      if (sy + sh > cloud.h) sh = cloud.h - sy;
      if (sw > 0 && sh > 0) {
        ctx.drawImage(cloud.canvas, sx, sy, sw, sh, dx, dy, sw, sh);
      }
    } else {
      const now = ratio();
      const k = (view.scale * now) / (cloud.scale * dpr);
      const dx = view.offsetX * now - cloud.offX * k;
      const dy = view.offsetY * now - cloud.offY * k;
      ctx.drawImage(cloud.canvas, dx, dy, cloud.w * k, cloud.h * k);
    }
    ctx.restore();
  }

  // Rebuilding costs tens of milliseconds, so it waits for the gesture to
  // stop rather than landing in the middle of one.
  let settle = 0;
  let builtAt = 0;
  function rebuildWhenIdle() {
    clearTimeout(settle);
    settle = setTimeout(() => {
      if (cloudStale()) {
        renderCloud();
        draw();
      }
    }, 90);
  }

  function draw() {
    if (!meta.has_layout) {
      ctx.clearRect(0, 0, width, height);
      ctx.fillStyle = "#8b90a3";
      ctx.font = "13px system-ui, sans-serif";
      ctx.fillText("No layout yet, run layout from settings", 20, 30);
      return;
    }

    // First paint has nothing to blit, so it pays for the buffer up front.
    // After that, a rebuild mid-gesture is only worth its cost when the drag
    // has run off the edge of the buffer and the alternative is bare panel.
    // Rate-limited, or a fast flick spends every frame rebuilding.
    if (!cloud.valid) {
      renderCloud();
    } else if (!cloudCovers() && performance.now() - builtAt > 120) {
      renderCloud();
    }
    blitCloud();
    if (cloudStale()) rebuildWhenIdle();

    const dim = route.length > 0;

    // The route's own points stay at full strength while the rest is dimmed.
    // A handful of points, so the normal canvas API is fine here.
    if (dim) {
      const radius = pointRadius(view.scale);
      ctx.globalAlpha = 0.8;
      for (const i of route) {
        ctx.beginPath();
        ctx.arc(screenX(i), screenY(i), radius, 0, TAU);
        ctx.fillStyle = PALETTE[genre[i] % PALETTE.length];
        ctx.fill();
      }
      ctx.globalAlpha = 1;
    }

    // Route polyline.
    if (route.length > 1) {
      ctx.beginPath();
      route.forEach((i, k) => {
        const [x, y] = toScreen(i);
        k === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
      });
      ctx.strokeStyle = "#ffffff";
      ctx.lineWidth = 1.5;
      ctx.stroke();

      route.forEach((i, k) => {
        const [x, y] = toScreen(i);
        ctx.beginPath();
        ctx.arc(x, y, 5, 0, TAU);
        ctx.fillStyle = k === 0 ? "#9ece6a" : k === route.length - 1 ? "#f7768e" : "#ffffff";
        ctx.fill();
      });
    }

    if (hoverCluster >= 0 && hoverCluster < clusters.length) clusterMarker(clusters[hoverCluster]);

    // Markers last, selection under hover, so the selected track reads as one
    // marker rather than two.
    if (selected >= 0) marker(selected, "#ffffff", true);
    if (hover >= 0 && hover !== selected) marker(hover, "#7aa2f7", false);

    placeCard();
  }

  // The selected point's card: Rust renders it, with the track's cover and
  // names, and this pins it over the point every frame, so it rides along
  // with a pan or a zoom. A DOM element over the canvas rather than something
  // drawn into it, so it keeps the same size on screen at any zoom and can be
  // clicked. The canvas fills its parent, so its coordinates are the card's.
  function placeCard() {
    const card = document.getElementById("map-card");
    if (!card) return;
    if (selected < 0 || width <= 0) {
      card.style.visibility = "hidden";
      return;
    }
    const [x, y] = toScreen(selected);
    const away = x < -40 || y < -40 || x > width + 40 || y > height + 40;
    card.style.visibility = away ? "hidden" : "visible";
    card.style.left = `${x}px`;
    card.style.top = `${y}px`;
  }

  /// A point worth looking at: halo, ring, core, and what it actually is.
  function marker(index, colour, persistent) {
    const [x, y] = toScreen(index);

    // Halo, so the marker separates from a dense cluster.
    ctx.beginPath();
    ctx.arc(x, y, persistent ? 15 : 12, 0, TAU);
    ctx.fillStyle = colour === "#ffffff"
      ? "rgba(255,255,255,0.12)"
      : "rgba(122,162,247,0.16)";
    ctx.fill();

    ctx.beginPath();
    ctx.arc(x, y, persistent ? 9 : 7.5, 0, TAU);
    ctx.strokeStyle = colour;
    ctx.lineWidth = 2;
    ctx.stroke();

    // A filled core keeps the marker legible once the ring is bigger than
    // the point it is ringing.
    ctx.beginPath();
    ctx.arc(x, y, 3, 0, TAU);
    ctx.fillStyle = colour;
    ctx.fill();

    // The selected point keeps a second, wider ring so it stays
    // distinguishable from whatever is merely under the cursor.
    if (persistent) {
      ctx.beginPath();
      ctx.arc(x, y, 13, 0, TAU);
      ctx.strokeStyle = colour;
      ctx.globalAlpha = 0.45;
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }

    // The selected point has its card instead.
    const text = (meta.labels && meta.labels[index]) || "";
    if (text && !persistent) label(text, x, y, colour);
  }

  /// A group under the cursor: a ring, and what it holds.
  function clusterMarker(cluster) {
    const x = cluster.x * view.scale + view.offsetX;
    const y = cluster.y * view.scale + view.offsetY;
    const r = cluster.glow * 0.45 * (view.scale / cloud.scale);
    ctx.beginPath();
    ctx.arc(x, y, r, 0, TAU);
    ctx.strokeStyle = "#ffffff";
    ctx.globalAlpha = 0.45;
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.globalAlpha = 1;

    const name = (meta.genres && meta.genres[cluster.genre]) || "unknown genre";
    const what = cluster.share >= 0.95 ? name : `mostly ${name}`;
    label(`${formatCount(cluster.count)} tracks · ${what}`, x, y, "#ffffff");
  }

  function roundRect(x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }

  /// Name the point, flipping the chip rather than clamping it, a chip
  /// pinned to the edge covers what you are trying to read.
  function label(text, x, y, colour) {
    ctx.font = "12px system-ui, -apple-system, sans-serif";
    const padX = 8;
    const h = 22;
    const maxWidth = Math.min(280, width - 16);
    let shown = text;
    let w = ctx.measureText(shown).width + padX * 2;
    if (w > maxWidth) {
      while (shown.length > 4 && ctx.measureText(shown + "…").width + padX * 2 > maxWidth) {
        shown = shown.slice(0, -1);
      }
      shown += "…";
      w = ctx.measureText(shown).width + padX * 2;
    }

    let lx = x + 16;
    let ly = y - h - 12;
    if (lx + w > width - 6) lx = x - w - 16;
    if (lx < 6) lx = 6;
    if (ly < 6) ly = y + 16;
    if (ly + h > height - 6) ly = height - h - 6;

    ctx.fillStyle = "rgba(18,19,26,0.94)";
    roundRect(lx, ly, w, h, 6);
    ctx.fill();
    ctx.strokeStyle = colour;
    ctx.globalAlpha = 0.7;
    ctx.lineWidth = 1;
    ctx.stroke();
    ctx.globalAlpha = 1;

    ctx.fillStyle = "#e5e7ef";
    ctx.textBaseline = "middle";
    ctx.fillText(shown, lx + padX, ly + h / 2);
  }

  /// Nearest point to a screen position, searched through the grid. This runs
  /// on every mousemove, so it must not depend on the size of the corpus:
  /// only the cells within `maxDistance` are visited.
  function nearest(px, py, maxDistance = 14) {
    if (!grid) return -1;
    const reach = maxDistance / view.scale;
    const lx = (px - view.offsetX) / view.scale;
    const ly = (py - view.offsetY) / view.scale;
    const cx0 = col(lx - reach);
    const cx1 = col(lx + reach);
    const cy0 = row(ly - reach);
    const cy1 = row(ly + reach);

    const { side, start, order } = grid;
    let best = -1;
    let bestDistance = maxDistance * maxDistance;
    for (let cy = cy0; cy <= cy1; cy++) {
      const base = cy * side;
      const from = start[base + cx0];
      const to = start[base + cx1 + 1];
      for (let k = from; k < to; k++) {
        const i = order[k];
        // Inside a group: the group answers for it, see `clusterAt`.
        if (hidden[i]) continue;
        const dx = screenX(i) - px;
        const dy = screenY(i) - py;
        const d = dx * dx + dy * dy;
        if (d < bestDistance) {
          bestDistance = d;
          best = i;
        }
      }
    }
    return best;
  }

  /// What a pointer at a screen position is on. A star right under it wins,
  /// since crowded patches still show some; then the group whose glow it is
  /// in; then the nearest star within the usual reach.
  function pick(px, py, reach = 14, slop = 3) {
    const close = nearest(px, py, Math.min(6, reach));
    if (close >= 0) return { point: close, group: -1 };
    const group = clusterAt(px, py, slop);
    if (group >= 0) return { point: -1, group };
    return { point: nearest(px, py, reach), group: -1 };
  }

  let dragging = false;
  let dragMoved = false;
  let last = { x: 0, y: 0 };

  canvas.addEventListener("mousedown", (e) => {
    // The left button only: the side ones are back and forward, see back.js.
    if (e.button !== 0) return;
    flight++;
    dragging = true;
    dragMoved = false;
    last = { x: e.offsetX, y: e.offsetY };
  });

  canvas.addEventListener("mousemove", (e) => {
    if (dragging) {
      view.offsetX += e.offsetX - last.x;
      view.offsetY += e.offsetY - last.y;
      last = { x: e.offsetX, y: e.offsetY };
      dragMoved = true;
      schedule();
      return;
    }
    pointer = { x: e.offsetX, y: e.offsetY };
    const { point: found, group } = pick(e.offsetX, e.offsetY);
    if (found !== hover || group !== hoverCluster) {
      hover = found;
      hoverCluster = group;
      canvas.style.cursor = found >= 0 || group >= 0 ? "pointer" : "grab";
      schedule();
    }
  });

  canvas.addEventListener("mouseleave", () => {
    pointer = null;
    if (hover < 0 && hoverCluster < 0) return;
    hover = -1;
    hoverCluster = -1;
    schedule();
  });

  window.addEventListener("mouseup", () => {
    dragging = false;
  });

  canvas.addEventListener("click", (e) => {
    if (dragMoved) return;
    const { point: found, group } = pick(e.offsetX, e.offsetY);
    if (group >= 0) {
      zoomTo(clusters[group]);
      return;
    }
    if (found >= 0) {
      selected = found;
      schedule();
      // Small message: exactly what eval is for.
      dioxus.send({ type: "select", track_id: ids[found] });
    } else if (selected >= 0) {
      // Clicking the background clears the selection, the way clicking beside
      // a list clears that. Guarded on there being one, so an idle click on
      // empty space does not wake every effect watching the selection.
      selected = -1;
      schedule();
      dioxus.send({ type: "select", track_id: null });
    }
  });

  // Right-click a point for the same actions its row has. The canvas is the
  // one surface with no rows to right-click, which left the map able to
  // select a track and nothing else.
  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const found = nearest(e.offsetX, e.offsetY);
    if (found < 0) return;
    selected = found;
    schedule();
    dioxus.send({ type: "menu", track_id: ids[found], x: e.clientX, y: e.clientY });
  });

  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    flight++;
    const factor = Math.exp(-e.deltaY * 0.001);
    view.offsetX = e.offsetX - (e.offsetX - view.offsetX) * factor;
    view.offsetY = e.offsetY - (e.offsetY - view.offsetY) * factor;
    view.scale *= factor;
    schedule();
  }, { passive: false });

  // ------------------------------------------------------------------ touch
  //
  // A webview synthesises click from tap, but not pan or zoom. One finger
  // pans, two pinch; tap sets `hover`, the only way to see a label.

  let touchStart = null;
  // Finger separation at the last sample, and how many fingers it was measured
  // from, a delta is only meaningful within one gesture shape.
  let pinch = 0;
  let touchCount = 0;

  function centre(touches) {
    const rect = canvas.getBoundingClientRect();
    let x = 0;
    let y = 0;
    for (const t of touches) {
      x += t.clientX - rect.left;
      y += t.clientY - rect.top;
    }
    return { x: x / touches.length, y: y / touches.length };
  }

  function spread(touches) {
    const dx = touches[0].clientX - touches[1].clientX;
    const dy = touches[0].clientY - touches[1].clientY;
    return Math.hypot(dx, dy);
  }

  /// Re-measure the gesture from the fingers that are down right now.
  function reseed(touches) {
    if (touches.length === 0) {
      pinch = 0;
      touchCount = 0;
      return;
    }
    last = centre(touches);
    pinch = touches.length >= 2 ? spread(touches) : 0;
    touchCount = touches.length;
  }

  // Long press is the touch equivalent of the right-click above. The
  // document-wide handler in long-press.js only fires inside `[data-menu]`,
  // which a canvas has no way to be: a point is a coordinate, not an element.
  let pressTimer = null;
  let pressFired = false;

  const cancelPress = () => {
    if (pressTimer !== null) clearTimeout(pressTimer);
    pressTimer = null;
  };

  canvas.addEventListener("touchstart", (e) => {
    // A gesture starts with the first finger. Later fingers must not restart
    // it, or a pinch gets counted as a tap.
    if (touchCount === 0) {
      flight++;
      dragMoved = false;
      touchStart = centre(e.touches);
      pressFired = false;
      cancelPress();
      if (e.touches.length === 1) {
        const point = { x: touchStart.x, y: touchStart.y };
        const client = { x: e.touches[0].clientX, y: e.touches[0].clientY };
        pressTimer = setTimeout(() => {
          pressTimer = null;
          if (dragMoved) return;
          const found = nearest(point.x, point.y, 22);
          if (found < 0) return;
          pressFired = true;
          selected = found;
          hover = found;
          schedule();
          dioxus.send({ type: "menu", track_id: ids[found], x: client.x, y: client.y });
        }, 500);
      }
    } else {
      // A second finger means a pinch, not a press.
      cancelPress();
    }
    reseed(e.touches);
  }, { passive: true });

  canvas.addEventListener("touchmove", (e) => {
    e.preventDefault();

    // Finger count changed, so `last` describes a different gesture.
    // Re-measure rather than subtracting incomparable positions, that jumped
    // the map by half the finger separation when one finger left a pinch.
    if (e.touches.length !== touchCount) {
      reseed(e.touches);
      return;
    }

    if (e.touches.length >= 2) {
      const now = spread(e.touches);
      const mid = centre(e.touches);
      if (pinch > 0 && now > 0) {
        const factor = now / pinch;
        view.offsetX = mid.x - (mid.x - view.offsetX) * factor;
        view.offsetY = mid.y - (mid.y - view.offsetY) * factor;
        view.scale *= factor;
      }
      // Two fingers pan as well as zoom, by however far their midpoint went.
      view.offsetX += mid.x - last.x;
      view.offsetY += mid.y - last.y;
      pinch = now;
      last = mid;
      dragMoved = true;
      schedule();
      return;
    }

    const at = centre(e.touches);
    view.offsetX += at.x - last.x;
    view.offsetY += at.y - last.y;
    last = at;
    // A few pixels of slop, or every tap counts as a drag and never selects.
    if (touchStart && Math.hypot(at.x - touchStart.x, at.y - touchStart.y) > 8) {
      dragMoved = true;
    }
    schedule();
  }, { passive: false });

  canvas.addEventListener("touchend", (e) => {
    // Fingers still down: the gesture continues in a new shape, so re-measure
    // from what remains instead of carrying the old midpoint forward.
    if (e.touches.length > 0) {
      reseed(e.touches);
      return;
    }

    reseed(e.touches);
    cancelPress();
    // The press already opened a menu; ending it must not also select.
    if (pressFired) {
      pressFired = false;
      return;
    }
    if (dragMoved || !touchStart) return;

    const { point: found, group } = pick(touchStart.x, touchStart.y, 22, 8);
    if (group >= 0) {
      zoomTo(clusters[group]);
      return;
    }
    if (found >= 0) {
      selected = found;
      // No cursor means no hover; showing the label for what was just tapped
      // is the closest equivalent.
      hover = found;
      schedule();
      dioxus.send({ type: "select", track_id: ids[found] });
    } else if (selected >= 0) {
      selected = -1;
      hover = -1;
      schedule();
      dioxus.send({ type: "select", track_id: null });
    }
  }, { passive: true });

  // Android hands the gesture to the system mid-flight often enough that a
  // missing handler here leaves `touchCount` stale until the next touchstart.
  canvas.addEventListener("touchcancel", (e) => {
    reseed(e.touches);
    cancelPress();
    dragMoved = true;
  }, { passive: true });

  // Rust calls this when the selection changes elsewhere, the in-space list,
  // an "in space" button, a generated route.
  window.stellySetSelected = (trackId) => {
    if (trackId === null || trackId === undefined) {
      selected = -1;
      schedule();
      return;
    }

    const found = byId.get(trackId);
    if (found === undefined) return;

    selected = found;

    // Bring it into view, but do not re-centre a point that is already
    // plainly visible.
    const x = screenX(found);
    const y = screenY(found);
    const margin = 40;
    // A hidden pane measures 0x0; leave the view alone rather than centring
    // on a rectangle that does not exist yet.
    if (width > 0 && height > 0 &&
        (x < margin || y < margin || x > width - margin || y > height - margin)) {
      view.offsetX += width / 2 - x;
      view.offsetY += height / 2 - y;
    }

    schedule();
  };

  // Rust calls this when a path is built. Ids only, tiny payload.
  window.stellySetRoute = (trackIds) => {
    route = trackIds.map((id) => byId.get(id)).filter((i) => i !== undefined);
    schedule();
  };

  // Rust calls this after the block list changes. Selection and route are
  // dropped: their indices referred to the old point set.
  window.stellyReloadPoints = async () => {
    await loadPoints();
    selected = -1;
    hover = -1;
    route = [];
    // The first real points, if the map was opened before they arrived.
    if (!fitted) fitted = fit();
    schedule();
    // The indices changed, but the track id did not; re-resolve it rather
    // than leaving the map unmarked after every rebuild.
    const wanted = window.stellySelected;
    if (wanted !== undefined && wanted !== null) window.stellySetSelected(wanted);
  };

  window.addEventListener("resize", resize);

  // The canvas can resize without the window doing so: opening the map
  // with `display: none` takes it from 0x0 to full with no resize event. `fit`
  // waits for a real rect for the same reason.
  function measured() {
    const rect = canvas.getBoundingClientRect();
    if (rect.width <= 0 || rect.height <= 0) return;
    if (!fitted) fitted = fit();
    resize();
  }

  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(measured).observe(canvas);
  }

  measured();

  // Rust may have chosen a track before this file finished loading; it leaves
  // the id here for exactly that case.
  function applyPendingSelection() {
    const wanted = window.stellySelected;
    if (wanted !== undefined && wanted !== null) {
      window.stellySetSelected(wanted);
    }
  }
  applyPendingSelection();

  // Keep the channel open so Rust can keep receiving selections.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
