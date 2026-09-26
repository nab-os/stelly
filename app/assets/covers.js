// Lazy cover loading, delegated.
//
// Every cover box renders with `data-cover="<url>"` and no background. This
// promotes it to a real background once the box comes within a screen of the
// viewport, then drops the attribute so it is never considered again.
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
  if (window.twoKhzCoversInstalled) return;
  window.twoKhzCoversInstalled = true;

  const PENDING = "[data-cover]:not([data-cover=''])";

  const load = (el) => {
    const url = el.dataset.cover;
    if (!url) return;
    el.style.backgroundImage = `url('${url}')`;
    // Once promoted it must stop matching the selector, or every rescan
    // walks every cover ever loaded.
    el.removeAttribute("data-cover");
  };

  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        observer.unobserve(entry.target);
        load(entry.target);
      }
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
  // whole lists.
  new MutationObserver((records) => {
    for (const record of records) {
      for (const node of record.addedNodes) scan(node);
    }
  }).observe(document.body, { childList: true, subtree: true });

  // The eval channel closes when this returns, and the observers die with it.
  (async () => {
    for (;;) await new Promise((resolve) => setTimeout(resolve, 1000));
  })();
})();
