// The cover flies between a tile and the page it opens.
//
// Opening an album, artist or track from a tile or row carries that row's
// cover up into the page's hero, and leaving the page carries it back down
// to wherever the same cover sits in what replaces it, if it is on screen.
//
// Delegated, like covers.js, rather than wired into each screen: Rust changes
// the view and Dioxus patches the DOM some time later, so nothing on the Rust
// side knows when both ends of the flight exist at once. The document does:
// the click says where the flight starts, and the hero, or on the way back
// the tile, turning up in a mutation says where it ends.
//
// Only a clone flies. The real covers are hidden underneath until it lands,
// so nothing Dioxus owns is ever moved or restyled, beyond `visibility`.
(() => {
  if (window.stellyTransitionsInstalled) return;
  window.stellyTransitionsInstalled = true;

  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
  const DURATION = 320;
  const EASING = "cubic-bezier(0.2, 0, 0, 1)";
  // How long after a click a hero still counts as the page it opened. A
  // click that plays a track instead of opening one leaves this pending, and
  // it must not attach to whatever page is opened next.
  const WINDOW = 1000;

  const urlOf = (cover) => {
    if (cover.dataset.cover) return cover.dataset.cover;
    const match = /url\(["']?(.*?)["']?\)/.exec(cover.style.backgroundImage || "");
    return match ? match[1] : "";
  };

  const snapshot = (cover) => {
    const style = getComputedStyle(cover);
    return {
      rect: cover.getBoundingClientRect(),
      image: style.backgroundImage,
      radius: style.borderRadius,
      url: urlOf(cover),
      at: performance.now(),
    };
  };

  const fresh = (start) => start && performance.now() - start.at < WINDOW;

  const visible = (rect) => {
    const body = document.querySelector(".screen-body");
    const bounds = body ? body.getBoundingClientRect() : { top: 0, bottom: innerHeight };
    return rect.width > 0 && rect.bottom > bounds.top && rect.top < bounds.bottom;
  };

  // The clone goes up and the real cover is hidden in the same task as the
  // mutation that brought the page in, so the frame that paints the new page
  // already has it. Waiting for the next frame to do either left one frame
  // with neither the old page nor any cover. Only measuring where it lands
  // waits: the scroll back to the top goes over the same channel as the new
  // page, and may land just after it.
  const fly = (from, target, { now = false, done } = {}) => {
    const clone = document.createElement("div");
    clone.className = "cover cover-flight";
    Object.assign(clone.style, {
      left: `${from.rect.left}px`,
      top: `${from.rect.top}px`,
      width: `${from.rect.width}px`,
      height: `${from.rect.height}px`,
      borderRadius: from.radius,
      backgroundImage: from.image !== "none" ? from.image : getComputedStyle(target).backgroundImage,
    });
    document.body.appendChild(clone);
    target.style.visibility = "hidden";
    // A tile still waiting on covers.js would land as the placeholder tint
    // and only then get its art. The clone carries the same url, already in
    // the cache, so the tile can have it now.
    if (!target.style.backgroundImage && from.image !== "none") {
      target.style.backgroundImage = from.image;
    }
    if (now) launch(clone, from, target, done);
    else requestAnimationFrame(() => launch(clone, from, target, done));
  };

  const launch = (clone, from, target, done) => {
    if (!target.isConnected) {
      target.style.visibility = "";
      clone.remove();
      return;
    }
    const to = target.getBoundingClientRect();
    const toRadius = getComputedStyle(target).borderRadius;
    const flight = clone.animate(
      [
        {
          left: clone.style.left,
          top: clone.style.top,
          width: clone.style.width,
          height: clone.style.height,
          borderRadius: from.radius,
        },
        {
          left: `${to.left}px`,
          top: `${to.top}px`,
          width: `${to.width}px`,
          height: `${to.height}px`,
          borderRadius: toRadius,
        },
      ],
      { duration: DURATION, easing: EASING, fill: "forwards" }
    );
    const land = () => {
      target.style.visibility = "";
      // Faded rather than dropped: the page may show a larger copy of the
      // art than the tile did, and swapping one for the other is a blink.
      clone.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 120 }).onfinish = () =>
        clone.remove();
      if (done) done();
    };
    flight.onfinish = land;
    flight.oncancel = land;
  };

  // Where the next page's cover flies from.
  let opening = null;
  // Where the current page's cover flies back to: the url it came from and,
  // kept up to date as the page scrolls, where the hero sits. By the time the
  // page's removal is seen the hero is gone and cannot be measured.
  let hero = null;
  let returning = null;

  const track = () => {
    if (hero && hero.el.isConnected) hero.last = snapshot(hero.el);
  };

  document.addEventListener(
    "click",
    (event) => {
      if (reduced.matches) return;
      const target = event.target;
      if (!(target instanceof Element)) return;
      // An artist or album name inside the row, or its menu button, goes
      // somewhere other than the row's own cover.
      if (target.closest(".clickable, button, a, input")) return;
      const row = target.closest("li");
      const cover = row && row.querySelector(".cover");
      opening = cover ? snapshot(cover) : null;
    },
    true
  );

  document.addEventListener("scroll", track, true);
  window.addEventListener("resize", track);

  const arrive = (el) => {
    const from = fresh(opening) ? opening : null;
    opening = null;
    hero = { el, url: from ? from.url : urlOf(el), last: null };
    if (!from) {
      requestAnimationFrame(track);
      return;
    }
    fly(from, el, { done: track });
  };

  const leave = () => {
    if (!hero || !hero.last || !hero.url) {
      hero = null;
      return;
    }
    returning = { ...hero.last, url: hero.url, at: performance.now() };
    hero = null;
  };

  // The shelf a page goes back to may still be loading when it mounts, so
  // the tile is looked for in every batch until the window closes.
  const land = () => {
    if (!fresh(returning)) {
      returning = null;
      return;
    }
    for (const cover of document.querySelectorAll(".screen-body .cover")) {
      if (urlOf(cover) !== returning.url) continue;
      const from = returning;
      returning = null;
      // Unlike the way in, nothing is hidden until the tile is known to be on
      // screen, once the scroll back to the top has landed: a clone put up
      // for a tile that then turns out to be out of sight is a flash. The
      // frame in between still shows the list, cover and all.
      requestAnimationFrame(() => {
        if (cover.isConnected && visible(cover.getBoundingClientRect())) {
          fly(from, cover, { now: true });
        }
      });
      return;
    }
  };

  const find = (node, selector) => {
    if (node.nodeType !== 1) return null;
    return node.matches(selector) ? node : node.querySelector(selector);
  };

  new MutationObserver((records) => {
    if (reduced.matches) return;
    let added = null;
    let removed = false;
    for (const record of records) {
      for (const node of record.removedNodes) {
        if (hero && (node === hero.el || (node.contains && node.contains(hero.el)))) removed = true;
      }
      for (const node of record.addedNodes) {
        added = added || find(node, ".hero-art");
      }
    }
    if (removed) leave();
    if (added) {
      // Page to page, album to artist: the new hero has its own way in, and
      // the old one has nowhere to go back to.
      returning = null;
      arrive(added);
    } else if (returning) {
      land();
    }
  }).observe(document.body, { childList: true, subtree: true });

  // The eval channel closes when this returns, and the observers die with it.
  (async () => {
    for (;;) await new Promise((resolve) => setTimeout(resolve, 1000));
  })();
})();
