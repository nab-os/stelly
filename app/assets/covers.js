// Lazy cover loading, delegated.
//
// Every cover box renders with `data-cover="<url>"` and no background. This
// promotes it to a real background once the box comes within a screen of the
// viewport, then drops the attribute so it is not considered again until it
// is given another.
//
// Delegated rather than per-row because the alternative is a hook per cover,
// and a favourites shelf holds hundreds. One observer and one mutation
// observer cover every list, grid and drawer in the window, including the ones
// that do not exist yet.
//
// The root margin is also the concurrency limit. WebKitGTK asked for 500
// images at once does not stay interactive; asked for a screenful at a time it
// does. Widen this only with a scroll test to back it up.
(() => {
  if (window.stellyCoversInstalled) return;
  window.stellyCoversInstalled = true;

  const PENDING = "[data-cover]:not([data-cover=''])";

  // A screenful at a time still means every cover *within* that screenful
  // fires its fetch in the same tick, which, for the grid views tiles
  // switched to by default, is a couple of dozen requests competing for the
  // handful of connections a browser keeps open per origin. Queuing behind a
  // small cap means the ones actually on screen still win that contest,
  // instead of racing evenly against ones that only happen to share their
  // 200px margin.
  const MAX_CONCURRENT = 6;
  let active = 0;
  // In arrival order, and only while near the viewport: a box that scrolls
  // back out before its turn leaves the line, stays observed, and rejoins if
  // it comes back. Otherwise a fast scroll leaves the covers it flew past
  // ahead of the ones it stopped on.
  const queue = new Set();
  // The URL each box last asked for, so a fetch that a newer cover on the same
  // box overtook does not land over it.
  const wanted = new WeakMap();

  const settle = () => {
    active--;
    pump();
  };

  const pump = () => {
    while (active < MAX_CONCURRENT && queue.size) {
      const el = queue.values().next().value;
      queue.delete(el);
      observer.unobserve(el);
      const url = el.dataset.cover;
      el.removeAttribute("data-cover");
      if (!url || !el.isConnected) continue;
      wanted.set(el, url);
      active++;
      // A plain `Image()` first: setting `backgroundImage` directly gives no
      // load/error event to queue behind, and preloading this way costs
      // nothing extra, the real fetch happens once, and the CSS assignment
      // below hits the browser's own cache.
      const probe = new Image();
      const apply = () => {
        if (wanted.get(el) === url) el.style.backgroundImage = `url('${url}')`;
      };
      probe.src = url;
      // Already in the cache: shown now, not a tick later, so a row that
      // re-renders does not flash its placeholder.
      if (probe.complete && probe.naturalWidth > 0) {
        apply();
        settle();
        continue;
      }
      // Decoded off the main thread before it is shown, where the engine can,
      // so the decode does not land in the middle of a scroll frame.
      if (probe.decode) {
        probe.decode().then(
          () => {
            apply();
            settle();
          },
          () => {
            // Some engines refuse to decode an image that did load; it can
            // still be shown, decoded on paint as before.
            if (probe.complete && probe.naturalWidth > 0) apply();
            settle();
          }
        );
      } else {
        probe.onload = () => {
          apply();
          settle();
        };
        probe.onerror = settle;
      }
    }
  };

  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) queue.add(entry.target);
        else queue.delete(entry.target);
      }
      pump();
    },
    { rootMargin: "200px" }
  );

  const scan = (root) => {
    if (!root || root.nodeType !== 1) return;
    if (root.matches && root.matches(PENDING)) observer.observe(root);
    const found = root.querySelectorAll ? root.querySelectorAll(PENDING) : [];
    for (const el of found) observer.observe(el);
  };

  scan(document.body);

  // Rows arrive as you scroll, search and navigate, so the set of covers is
  // never fixed. Subtree, because Dioxus patches deep rather than replacing
  // whole lists. The attribute too: a box Dioxus keeps but points at another
  // cover gets its `data-cover` back, and would otherwise never load it.
  new MutationObserver((records) => {
    for (const record of records) {
      if (record.type === "attributes") scan(record.target);
      else for (const node of record.addedNodes) scan(node);
    }
  }).observe(document.body, {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["data-cover"],
  });

  // The eval channel closes when this returns, and the observers die with it.
  (async () => {
    for (;;) await new Promise((resolve) => setTimeout(resolve, 1000));
  })();
})();
