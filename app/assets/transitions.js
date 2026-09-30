// Opening an album, artist or track page: its cover flies up out of the tile
// it was opened from, and the page opens out of a circle round that tile
// until it fills the main area. Going back runs it the other way, the page
// closing down into the tile its cover lands on.
//
// Rust calls `stellyLeaving` on every navigation, before the new page is
// rendered: an eval reaches the webview ahead of the render's DOM edits, so
// the old page is still there to be copied and measured. Everything after
// that is driven from here, off the mutations that bring the new page in.
//
// Neither end of the animation is anything Dioxus owns. The old page is a
// copy (the ghost) laid over the new one, the circle is a mask on it, and
// the flying cover is a copy too. The real covers underneath are only hidden
// with `visibility` until it lands.
(() => {
  if (window.stellyTransitionsInstalled) return;
  window.stellyTransitionsInstalled = true;

  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
  const DURATION = 320;
  const EASING = "cubic-bezier(0.2, 0, 0, 1)";
  // How long a navigation counts as the one the next page change belongs
  // to. A click that plays a track instead of opening one leaves its note
  // behind, and it must not attach to whatever page is opened next.
  const WINDOW = 1000;

  // EASING again, for the circle, which is stepped by hand: solve the
  // bezier's x for t, then read its y.
  const ease = (t) => {
    const x = (s) => 0.6 * s * (1 - s) ** 2 + s ** 3;
    const y = (s) => 3 * s ** 2 * (1 - s) + s ** 3;
    let lo = 0;
    let hi = 1;
    for (let i = 0; i < 20; i++) {
      const mid = (lo + hi) / 2;
      if (x(mid) < t) lo = mid;
      else hi = mid;
    }
    return y((lo + hi) / 2);
  };

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

  const fresh = (note) => note && performance.now() - note.at < WINDOW;
  const center = (rect) => ({ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 });
  const inGhost = (el) => !!el.closest(".page-ghost");
  const screen = () => document.querySelector(".screen-body");

  const visible = (rect) => {
    const body = screen();
    const bounds = body ? body.getBoundingClientRect() : { top: 0, bottom: innerHeight };
    return rect.width > 0 && rect.bottom > bounds.top && rect.top < bounds.bottom;
  };

  // Far enough from `c` to cover every corner of `rect`.
  const reach = (c, rect) =>
    Math.max(
      Math.hypot(c.x - rect.left, c.y - rect.top),
      Math.hypot(c.x - rect.right, c.y - rect.top),
      Math.hypot(c.x - rect.left, c.y - rect.bottom),
      Math.hypot(c.x - rect.right, c.y - rect.bottom)
    );

  // Resolves once the scroll the new page asked for has been applied, see
  // `scroll_to` in library.rs, or after a moment if none comes. Anything on
  // the new page is measured after this, or it is measured where the old
  // page's scroll left it.
  const settled = () =>
    new Promise((resolve) => {
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        window.removeEventListener("stelly-scrolled", finish);
        requestAnimationFrame(() => resolve());
      };
      window.addEventListener("stelly-scrolled", finish);
      setTimeout(finish, 250);
    });

  // ------------------------------------------------------------ the ghost

  // A copy of the main area as it is, to lay over the page that replaces it.
  // Taken on every navigation, put up only when something animates.
  const copyScreen = () => {
    const body = screen();
    if (!body) return null;
    let background = "";
    for (let el = body; el && !background; el = el.parentElement) {
      const bg = getComputedStyle(el).backgroundColor;
      if (bg && bg !== "transparent" && bg !== "rgba(0, 0, 0, 0)") background = bg;
    }
    const copy = body.cloneNode(true);
    // Dioxus finds its nodes by these; two of each is asking for trouble.
    copy.removeAttribute("data-dioxus-id");
    for (const el of copy.querySelectorAll("[data-dioxus-id]")) el.removeAttribute("data-dioxus-id");
    return { copy, rect: body.getBoundingClientRect(), scroll: body.scrollTop, background };
  };

  // Up over the new page, with the covers `hide` picks hidden: the one that
  // flies, so it is not also left behind in the copy.
  const raise = (ghost, hide) => {
    const frame = document.createElement("div");
    frame.className = "page-ghost";
    Object.assign(frame.style, {
      left: `${ghost.rect.left}px`,
      top: `${ghost.rect.top}px`,
      width: `${ghost.rect.width}px`,
      height: `${ghost.rect.height}px`,
      background: ghost.background,
    });
    ghost.copy.style.height = `${ghost.rect.height}px`;
    frame.appendChild(ghost.copy);
    document.body.appendChild(frame);
    ghost.copy.scrollTop = ghost.scroll;
    for (const cover of ghost.copy.querySelectorAll(".cover")) {
      if (hide(cover)) cover.style.visibility = "hidden";
    }
    return frame;
  };

  // The circle, as a mask on the ghost: a hole the new page shows through,
  // or a disc the old one shows through. Stepped by hand, a gradient's
  // radius is not something WebKitGTK will animate.
  const mask = (frame, c, hole) => {
    const box = frame.getBoundingClientRect();
    const [inside, outside] = hole ? ["transparent", "black"] : ["black", "transparent"];
    const r = Math.max(0, c.r);
    const image = `radial-gradient(circle at ${c.x - box.left}px ${c.y - box.top}px, ${inside} ${r}px, ${outside} ${r + 1}px)`;
    frame.style.webkitMaskImage = image;
    frame.style.maskImage = image;
  };

  const circle = (frame, from, to, hole) => {
    const start = performance.now();
    const step = (now) => {
      const t = Math.min(1, (now - start) / DURATION);
      const k = ease(t);
      mask(
        frame,
        {
          x: from.x + (to.x - from.x) * k,
          y: from.y + (to.y - from.y) * k,
          r: from.r + (to.r - from.r) * k,
        },
        hole
      );
      if (t < 1) requestAnimationFrame(step);
      else frame.remove();
    };
    mask(frame, from, hole);
    requestAnimationFrame(step);
  };

  // --------------------------------------------------------- the cover

  // Put up in the same task as the mutation that brought the page in, so
  // the frame that paints the new page already has it.
  const lift = (from) => {
    const clone = document.createElement("div");
    clone.className = "cover cover-flight";
    Object.assign(clone.style, {
      left: `${from.rect.left}px`,
      top: `${from.rect.top}px`,
      width: `${from.rect.width}px`,
      height: `${from.rect.height}px`,
      borderRadius: from.radius,
      backgroundImage: from.image,
    });
    document.body.appendChild(clone);
    return clone;
  };

  const fly = (clone, from, target) => {
    // A tile still waiting on covers.js would land as the placeholder tint
    // and only then get its art. The clone carries the same url, already in
    // the cache, so the tile can have it now.
    if (!target.style.backgroundImage && from.image !== "none") {
      target.style.backgroundImage = from.image;
    }
    const to = target.getBoundingClientRect();
    const flight = clone.animate(
      [
        {
          left: `${from.rect.left}px`,
          top: `${from.rect.top}px`,
          width: `${from.rect.width}px`,
          height: `${from.rect.height}px`,
          borderRadius: from.radius,
        },
        {
          left: `${to.left}px`,
          top: `${to.top}px`,
          width: `${to.width}px`,
          height: `${to.height}px`,
          borderRadius: getComputedStyle(target).borderRadius,
        },
      ],
      { duration: DURATION, easing: EASING, fill: "forwards" }
    );
    let landed = false;
    const land = () => {
      if (landed) return;
      landed = true;
      target.style.visibility = "";
      // Faded rather than dropped: the page may show a larger copy of the
      // art than the tile did, and swapping one for the other is a blink.
      clone.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 120 }).onfinish = () =>
        clone.remove();
    };
    flight.onfinish = land;
    flight.oncancel = land;
    return to;
  };

  // ------------------------------------------------------------- state

  // The tile clicked, if the navigation was a click on one: the surest
  // answer to where a page came out of, when the same album shows twice.
  let clicked = null;
  // What `stellyLeaving` noted: the kind of navigation, the ghost, where the
  // page's own cover was, and every cover on screen, which a page reached
  // any other way than a click (the mouse's forward button, say) picks the
  // one it came out of from by url.
  let leaving = null;
  // The page on screen with a cover: the cover, and the url of the tile it
  // came out of, to go back into.
  let hero = null;

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
      clicked = cover ? snapshot(cover) : null;
    },
    true
  );

  window.stellyLeaving = (kind) => {
    leaving = null;
    if (reduced.matches) return;
    const covers = new Map();
    for (const cover of document.querySelectorAll(".screen-body .cover")) {
      if (inGhost(cover)) continue;
      const url = urlOf(cover);
      if (!url || covers.has(url)) continue;
      const shot = snapshot(cover);
      if (visible(shot.rect)) covers.set(url, shot);
    }
    leaving = {
      kind,
      covers,
      ghost: copyScreen(),
      hero: hero && hero.el.isConnected ? { ...snapshot(hero.el), url: hero.url } : null,
      at: performance.now(),
    };
  };

  // --------------------------------------------------------- the moves

  const open = (el) => {
    const note = fresh(leaving) ? leaving : null;
    const tap = fresh(clicked) ? clicked : null;
    leaving = null;
    clicked = null;
    const from = tap || (note && note.covers.get(urlOf(el))) || null;
    hero = { el, url: from ? from.url : urlOf(el) };
    if (!from || !note || !note.ghost) return;

    const frame = raise(note.ghost, (cover) => urlOf(cover) === from.url);
    const clone = lift(from);
    el.style.visibility = "hidden";
    // The circle starts round the picture, so the page opens out of it.
    const start = { ...center(from.rect), r: Math.hypot(from.rect.width, from.rect.height) / 2 };
    mask(frame, start, true);

    settled().then(() => {
      if (!el.isConnected) {
        el.style.visibility = "";
        clone.remove();
        frame.remove();
        return;
      }
      fly(clone, from, el);
      circle(frame, start, { ...start, r: reach(start, frame.getBoundingClientRect()) }, true);
    });
  };

  const close = () => {
    const note = fresh(leaving) ? leaving : null;
    leaving = null;
    hero = null;
    if (!note || note.kind !== "back" || !note.ghost || !note.hero) return;

    const from = note.hero;
    const frame = raise(note.ghost, (cover) => cover.classList.contains("hero-art"));
    const clone = lift(from);
    const c = center(from.rect);
    const start = { ...c, r: reach(c, frame.getBoundingClientRect()) };
    mask(frame, start, false);

    settled().then(() => {
      let tile = null;
      for (const cover of document.querySelectorAll(".screen-body .cover")) {
        if (inGhost(cover) || urlOf(cover) !== from.url) continue;
        if (visible(cover.getBoundingClientRect())) tile = cover;
        break;
      }
      if (tile) {
        tile.style.visibility = "hidden";
        const to = fly(clone, from, tile);
        circle(frame, start, { ...center(to), r: Math.hypot(to.width, to.height) / 2 }, false);
      } else {
        // Nowhere on screen to go back into: the page closes down into
        // where its own cover was, and the cover fades with it.
        clone.animate([{ opacity: 1 }, { opacity: 0 }], { duration: DURATION, fill: "forwards" }).onfinish =
          () => clone.remove();
        circle(frame, start, { ...c, r: 0 }, false);
      }
    });
  };

  const find = (node, selector) => {
    if (node.nodeType !== 1 || inGhost(node)) return null;
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
    // Page to page, album to artist, is an `open`; only a page with a cover
    // giving way to one without is a `close`.
    if (added) open(added);
    else if (removed) close();
  }).observe(document.body, { childList: true, subtree: true });

  // The eval channel closes when this returns, and the observers die with it.
  (async () => {
    for (;;) await new Promise((resolve) => setTimeout(resolve, 1000));
  })();
})();
