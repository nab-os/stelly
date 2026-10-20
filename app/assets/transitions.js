// Opening an album, artist or track page: its cover flies up out of the tile
// it was opened from, and the page it left fades away over the new one.
// Going back runs it the other way, the cover landing on its tile as the
// page fades off the list.
//
// Rust calls `stellyLeaving` on every navigation, before the new page is
// rendered: an eval reaches the webview ahead of the render's DOM edits, so
// the old page is still there to be copied and measured. It also says where
// the new page will be scrolled to, so where everything lands is known the
// moment the new page is in, and nothing waits for the scroll to be put
// back before it starts moving. Everything after that is driven from here,
// off the mutations that bring the new page in.
//
// Neither end of the animation is anything Dioxus owns. The old page is a
// copy (the ghost) laid over the new one, and the flying cover is a copy
// too. The real covers underneath are only hidden with `visibility` until
// it lands. Both only ever move by `opacity` and `transform`, which the
// compositor animates on its own: anything else repaints every frame.
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

  // The last layer's URL: a hero lays a larger copy over the one its tile
  // shows, and it is that one the two have in common.
  const urlOf = (cover) => {
    if (cover.dataset.cover) return cover.dataset.cover;
    const match = /.*url\(["']?(.*?)["']?\)/.exec(cover.style.backgroundImage || "");
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

  // The main area's own covers, in document order. The ghost's copy of the
  // main area comes after it in the document, so `screen` is never it.
  const screenCovers = () => {
    const body = screen();
    return body ? [...body.querySelectorAll(".cover")] : [];
  };

  // Where a card sits in its list: its place among the covers, and its
  // position in the list's own coordinates, scroll taken out. Enough to find
  // that same card again when the list comes back, rather than any card with
  // the same art: an album's tracks all share its cover.
  const place = (cover, index) => {
    const body = screen();
    const rect = cover.getBoundingClientRect();
    const box = body.getBoundingClientRect();
    return {
      index,
      x: rect.left - box.left,
      y: rect.top - box.top + body.scrollTop,
      width: rect.width,
      height: rect.height,
      radius: getComputedStyle(cover).borderRadius,
    };
  };
  const inGhost = (el) => !!el.closest(".page-ghost");
  const screen = () => document.querySelector(".screen-body");

  const visible = (rect) => {
    const body = screen();
    const bounds = body ? body.getBoundingClientRect() : { top: 0, bottom: innerHeight };
    return rect.width > 0 && rect.top + rect.height > bounds.top && rect.top < bounds.bottom;
  };

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
    const rect = body.getBoundingClientRect();
    const copy = copyShown(body, rect);
    // Dioxus finds its nodes by these; two of each is asking for trouble.
    copy.removeAttribute("data-dioxus-id");
    for (const el of copy.querySelectorAll("[data-dioxus-id]")) el.removeAttribute("data-dioxus-id");
    return { copy, rect, scroll: body.scrollTop, background };
  };

  // What of `el` shows inside `view`. Whatever is wholly inside is copied
  // whole, and whatever is wholly outside becomes an empty box its size,
  // kept for the layout around it: a grid places a tile by how many came
  // before it. Only what straddles an edge is gone through child by child,
  // and nothing after the last thing that shows is kept, as nothing that
  // comes after a box moves it. A long list is then its screenful, not its
  // every row.
  const copyShown = (el, view) => {
    const copy = el.cloneNode(false);
    const children = [...el.childNodes];
    const seen = children.map((child) => (child.nodeType === 1 ? shows(child, view) : "in"));
    let end = -1;
    seen.forEach((where, i) => {
      if (where !== "out" && children[i].nodeType === 1) end = i;
    });
    children.slice(0, end + 1).forEach((child, i) => {
      if (seen[i] === "in") copy.appendChild(child.cloneNode(true));
      else if (seen[i] === "edge") copy.appendChild(copyShown(child, view));
      else copy.appendChild(placeholder(child));
    });
    return copy;
  };

  // "in", "out" or "edge" of `view`. A box with no size of its own, laid
  // out as its children (`display: contents`), counts as in.
  const shows = (el, view) => {
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 && rect.height === 0) return "in";
    if (rect.bottom <= view.top || rect.top >= view.bottom) return "out";
    if (rect.right <= view.left || rect.left >= view.right) return "out";
    const inside =
      rect.top >= view.top && rect.bottom <= view.bottom && rect.left >= view.left && rect.right <= view.right;
    return inside || !el.firstElementChild ? "in" : "edge";
  };

  const placeholder = (el) => {
    const rect = el.getBoundingClientRect();
    const box = el.cloneNode(false);
    Object.assign(box.style, {
      boxSizing: "border-box",
      width: `${rect.width}px`,
      height: `${rect.height}px`,
      visibility: "hidden",
    });
    return box;
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

  // The ghost fading off the new page. It used to open or close as a
  // circle, a mask stepped every frame; WebKitGTK repainted the whole ghost
  // for each step.
  const fade = (frame) => {
    frame.animate([{ opacity: 1 }, { opacity: 0 }], { duration: DURATION, easing: EASING, fill: "forwards" }).onfinish =
      () => frame.remove();
  };

  // --------------------------------------------------------- the cover

  // `rect`, for a box laid out at `at`, as a transform.
  const onto = (at, rect) =>
    `translate(${rect.left - at.left}px, ${rect.top - at.top}px) scale(${rect.width / at.width}, ${rect.height / at.height})`;

  // Put up in the same task as the mutation that brought the page in, so
  // the frame that paints the new page already has it. Laid out the size of
  // the larger end, `from` or `to`, and scaled down to the other: scaled up,
  // it would be drawn small and blurred. Its corners are the larger end's
  // too, scaled with it, as rounding them as it goes is a repaint.
  const lift = (from, to) => {
    const large = to && to.width * to.height > from.rect.width * from.rect.height;
    const at = large ? to : from.rect;
    const clone = document.createElement("div");
    clone.className = "cover cover-flight";
    Object.assign(clone.style, {
      left: `${at.left}px`,
      top: `${at.top}px`,
      width: `${at.width}px`,
      height: `${at.height}px`,
      borderRadius: large ? to.radius : from.radius,
      backgroundImage: from.image,
      transform: onto(at, from.rect),
    });
    document.body.appendChild(clone);
    return { el: clone, at };
  };

  // To `to`, worked out ahead rather than measured, and into whatever
  // `find` turns up there: hidden until the clone lands on it, and given its
  // art now if covers.js has not got to it yet. A list still being filled in
  // may only bring the card in mid-flight, so it is asked again each frame.
  const fly = (clone, from, to, find) => {
    if (!clone.at.width || !clone.at.height) {
      clone.el.remove();
      return;
    }
    let target = null;
    let landed = false;
    const claim = () => {
      target = find();
      if (!target) return;
      if (!target.style.backgroundImage && from.image !== "none") {
        target.style.backgroundImage = from.image;
      }
      target.style.visibility = "hidden";
    };
    const watch = () => {
      if (landed || target) return;
      claim();
      requestAnimationFrame(watch);
    };
    watch();

    const flight = clone.el.animate(
      [{ transform: onto(clone.at, from.rect) }, { transform: onto(clone.at, to) }],
      { duration: DURATION, easing: EASING, fill: "forwards" }
    );
    const land = () => {
      if (landed) return;
      landed = true;
      if (target) target.style.visibility = "";
      // Faded rather than dropped: the page may show a larger copy of the
      // art than the tile did, and swapping one for the other is a blink.
      clone.el.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 120 }).onfinish = () =>
        clone.el.remove();
    };
    flight.onfinish = land;
    flight.oncancel = land;
  };

  // ------------------------------------------------------------- state

  // The tile clicked, if the navigation was a click on one: the surest
  // answer to where a page came out of, when the same album shows twice.
  // Only the cover until `stellyLeaving` measures it: most clicks on a row
  // play it rather than open anything.
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
      clicked = cover ? { cover, at: performance.now() } : null;
    },
    true
  );

  window.stellyLeaving = (kind, scroll) => {
    leaving = null;
    if (reduced.matches) return;
    // Only the covers in view are measured in full; the rest of a long
    // list is a rectangle each, read off a layout already done.
    const covers = new Map();
    const tap = fresh(clicked) ? clicked.cover : null;
    clicked = null;
    screenCovers().forEach((cover, index) => {
      if (cover === tap) clicked = { ...snapshot(cover), place: place(cover, index) };
      const url = urlOf(cover);
      if (!url || covers.has(url) || !visible(cover.getBoundingClientRect())) return;
      covers.set(url, { ...snapshot(cover), place: place(cover, index) });
    });
    // A tile outside the main area, the queue's say, has no place in it.
    if (tap && !clicked && tap.isConnected) clicked = { ...snapshot(tap), place: place(tap, -1) };
    leaving = {
      kind,
      scroll,
      covers,
      ghost: copyScreen(),
      hero: hero && hero.el.isConnected ? { ...snapshot(hero.el), url: hero.url, place: hero.place } : null,
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
    hero = { el, url: from ? from.url : urlOf(el), place: from ? from.place : null };
    if (!from || !note || !note.ghost) return;

    const frame = raise(note.ghost, (cover) => urlOf(cover) === from.url);

    // Where the cover sits once the page has its own scroll: for now the
    // main area still has the old page's, cut short to fit the new one.
    const body = screen();
    const shift = body && note.scroll != null ? body.scrollTop - note.scroll : 0;
    const at = el.getBoundingClientRect();
    const to = {
      left: at.left,
      top: at.top + shift,
      width: at.width,
      height: at.height,
      radius: getComputedStyle(el).borderRadius,
    };
    fly(lift(from, to), from, to, () => (el.isConnected ? el : null));
    fade(frame);
  };

  const close = () => {
    const note = fresh(leaving) ? leaving : null;
    leaving = null;
    hero = null;
    if (!note || note.kind !== "back" || !note.ghost || !note.hero) return;

    const from = note.hero;
    const frame = raise(note.ghost, (cover) => cover.classList.contains("hero-art"));
    fade(frame);

    // The card it came out of, where it will be once the list has its
    // scroll back: its place in the list, less that scroll.
    const spot = from.place;
    const box = screen().getBoundingClientRect();
    const to = spot && {
      left: box.left + spot.x,
      top: box.top + spot.y - (note.scroll != null ? note.scroll : 0),
      width: spot.width,
      height: spot.height,
      radius: spot.radius,
    };
    if (to && visible(to)) {
      // The same card if the list still has it at that place; if the list
      // has changed since, the cover lands where it was and fades.
      const card = () => {
        const cover = screenCovers()[spot.index];
        return cover && urlOf(cover) === from.url ? cover : null;
      };
      fly(lift(from, to), from, to, card);
    } else {
      // Nowhere on screen to go back into: the cover fades where it was,
      // with the page.
      const clone = lift(from);
      clone.el.animate([{ opacity: 1 }, { opacity: 0 }], { duration: DURATION, fill: "forwards" }).onfinish = () =>
        clone.el.remove();
    }
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
