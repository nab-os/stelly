// Gestures for the queue drawer: reorder by dragging a row, move a whole
// group (an album, a generated list) by dragging its header, remove a row by
// swiping it away, close the drawer by its handle.
//
// A mouse drags a row as soon as it moves a few pixels with the button held;
// a plain click still plays it. A finger has to hold still first (a long
// press), because a finger moving straight away is scrolling the list. A
// finger moving sideways instead swipes the row out of the queue.
//
// Touch events rather than pointer events on a touch screen: once a row is
// being dragged the list must stop scrolling, and cancelling `touchmove` is
// the one way to do that which Android's WebView honours mid-gesture. A
// pointer-event drag there gets a `pointercancel` the moment the browser
// decides the finger is scrolling.
//
// Nothing here reorders the DOM. Rows are translated to show where the
// dragged one would land, then `{from, to}` goes to Rust, which owns the
// queue and re-renders it in the new order. A group goes as
// `{group, to}`, `to` being the row it lands before.

window.stellyQueueSend = (message) => dioxus.send(message);

if (!window.stellyQueueGesturesInstalled) {
  window.stellyQueueGesturesInstalled = true;

  const HOLD_MS = 350;
  const SLOP_PX = 8;
  const MOUSE_SLOP_PX = 4;

  const send = (message) => window.stellyQueueSend && window.stellyQueueSend(message);

  const rowsOf = (list) =>
    Array.from(list.children).filter((node) => node.tagName === "LI");

  // Buttons and names inside a row do their own thing; a press on one of
  // them is never the start of a drag.
  const rowFor = (target) => {
    if (!target.closest || target.closest("button, .clickable")) return null;
    const row = target.closest(".queue-list > li");
    return row || null;
  };

  // A group's header, which drags the group.
  const headFor = (target) => {
    if (!target.closest || target.closest("button, .clickable")) return null;
    return target.closest(".queue-list > .group-head");
  };

  // What a press can drag: a row, or a group by its header.
  const grabFor = (target) => rowFor(target) || headFor(target);

  // The list as what a group can be dragged past: each group's run, its
  // header, whatever follows it, and its rows, as one; each row outside a
  // group on its own. `first` is the unit's first row among the `li`s.
  const unitsOf = (list) => {
    const units = [];
    let rows = 0;
    const children = Array.from(list.children);
    for (let i = 0; i < children.length; i++) {
      const node = children[i];
      if (node.classList.contains("group-head")) {
        const nodes = [node];
        let j = i + 1;
        while (j < children.length && children[j].tagName !== "LI" && !children[j].classList.contains("group-head")) {
          nodes.push(children[j++]);
        }
        const count = Number(node.dataset.rows) || 0;
        for (let k = 0; k < count && j < children.length && children[j].tagName === "LI"; k++) {
          nodes.push(children[j++]);
        }
        units.push({ nodes, first: rows, head: node });
        rows += nodes.filter((n) => n.tagName === "LI").length;
        i = j - 1;
      } else if (node.tagName === "LI") {
        units.push({ nodes: [node], first: rows });
        rows += 1;
      } else if (units.length) {
        units[units.length - 1].nodes.push(node);
      }
    }
    return { units, rows };
  };

  const height = (nodes) => nodes.reduce((sum, node) => sum + node.getBoundingClientRect().height, 0);

  // A drag ends on a mouseup over the row it started on, which the webview
  // then reports as a click on that row: play it. Swallow that one click.
  let swallowClick = false;
  document.addEventListener(
    "click",
    (event) => {
      if (!swallowClick) return;
      swallowClick = false;
      event.stopPropagation();
      event.preventDefault();
    },
    true
  );

  // ------------------------------------------------------------ reordering

  let drag = null;

  // A row, or with a header a group, picked up at `y`. Either way the drag is
  // of one unit among others: rows among rows, or a group among the groups
  // and loose rows around it.
  function startDrag(grabbed, y) {
    const list = grabbed.parentElement;
    let units;
    let from;
    let rows = 0;
    if (grabbed.classList.contains("group-head")) {
      const all = unitsOf(list);
      units = all.units;
      rows = all.rows;
      from = units.findIndex((unit) => unit.head === grabbed);
    } else {
      units = rowsOf(list).map((row, i) => ({ nodes: [row], first: i }));
      from = units.findIndex((unit) => unit.nodes[0] === grabbed);
    }
    if (from < 0) return false;
    drag = {
      grabbed,
      group: grabbed.dataset.group,
      from,
      to: from,
      startY: y,
      units,
      rows,
      // Measured once: re-measuring per move would read rows this drag has
      // already translated.
      heights: units.map((unit) => height(unit.nodes)),
    };
    units[from].nodes.forEach((node) => node.classList.add("dragging"));
    if (navigator.vibrate) navigator.vibrate(12);
    return true;
  }

  const shift = (unit, px) =>
    unit.nodes.forEach((node) => {
      node.style.transform = px ? `translateY(${px}px)` : "";
    });

  function moveDrag(y) {
    const offset = y - drag.startY;
    shift(drag.units[drag.from], offset);

    // Walk outward from the original slot, consuming neighbour heights, until
    // the pointer no longer clears the next unit's midpoint.
    let to = drag.from;
    if (offset > 0) {
      let edge = 0;
      for (let i = drag.from + 1; i < drag.units.length; i++) {
        if (offset <= edge + drag.heights[i] / 2) break;
        edge += drag.heights[i];
        to = i;
      }
    } else if (offset < 0) {
      let edge = 0;
      for (let i = drag.from - 1; i >= 0; i--) {
        if (-offset <= edge + drag.heights[i] / 2) break;
        edge += drag.heights[i];
        to = i;
      }
    }
    drag.to = to;

    const gap = drag.heights[drag.from];
    drag.units.forEach((unit, i) => {
      if (i === drag.from) return;
      let px = 0;
      if (to > drag.from && i > drag.from && i <= to) px = -gap;
      if (to < drag.from && i >= to && i < drag.from) px = gap;
      shift(unit, px);
    });
  }

  function endDrag() {
    const { from, to, units, group, rows } = drag;
    drag = null;
    // Cleared before reporting: Rust re-renders these rows in the new order,
    // and a stale transform would offset whatever lands in that slot.
    units.forEach((unit) => {
      shift(unit, 0);
      unit.nodes.forEach((node) => node.classList.remove("dragging"));
    });
    if (from === to) return;
    if (group === undefined) {
      send({ type: "move", from, to });
    } else {
      // The row it lands before: the dropped-on unit's first going up, the
      // one after it going down, or past the end.
      const before = to < from ? units[to].first : to + 1 < units.length ? units[to + 1].first : rows;
      send({ type: "move-group", group: Number(group), to: before });
    }
  }

  // ---------------------------------------------------------------- mouse

  let mouse = null;

  document.addEventListener("pointerdown", (event) => {
    if (event.pointerType !== "mouse" || event.button !== 0) return;
    const row = grabFor(event.target);
    if (!row) return;
    mouse = { row, y: event.clientY, dragging: false };
  });

  document.addEventListener("pointermove", (event) => {
    if (!mouse || event.pointerType !== "mouse") return;
    if (!mouse.dragging) {
      if (Math.abs(event.clientY - mouse.y) < MOUSE_SLOP_PX) return;
      if (!startDrag(mouse.row, mouse.y)) {
        mouse = null;
        return;
      }
      mouse.dragging = true;
    }
    event.preventDefault();
    moveDrag(event.clientY);
  });

  const mouseUp = () => {
    if (!mouse) return;
    if (mouse.dragging && drag) {
      endDrag();
      swallowClick = true;
      setTimeout(() => { swallowClick = false; }, 300);
    }
    mouse = null;
  };
  document.addEventListener("pointerup", mouseUp);
  document.addEventListener("pointercancel", mouseUp);

  // ---------------------------------------------------------------- touch

  let touch = null;

  const resetSwipe = (row) => {
    row.style.transition = "";
    row.style.transform = "";
    row.style.opacity = "";
  };

  document.addEventListener(
    "touchstart",
    (event) => {
      if (event.touches.length !== 1) return;
      const row = grabFor(event.target);
      if (!row) return;
      const point = event.touches[0];
      touch = {
        row,
        x: point.clientX,
        y: point.clientY,
        mode: "pending",
        timer: setTimeout(() => {
          if (touch && touch.mode === "pending" && startDrag(touch.row, touch.y)) {
            touch.mode = "drag";
          }
        }, HOLD_MS),
      };
    },
    { passive: true }
  );

  const rowMove = (event) => {
    if (!touch) return;
    const point = event.touches[0];
    const dx = point.clientX - touch.x;
    const dy = point.clientY - touch.y;

    if (touch.mode === "pending") {
      if (Math.abs(dx) < SLOP_PX && Math.abs(dy) < SLOP_PX) return;
      clearTimeout(touch.timer);
      if (Math.abs(dx) > Math.abs(dy) && touch.row.tagName === "LI") {
        touch.mode = "swipe";
        touch.row.style.transition = "none";
      } else {
        // Moving before the hold: this is a scroll, leave it alone.
        touch = null;
        return;
      }
    }

    if (touch.mode === "drag") {
      event.preventDefault();
      moveDrag(point.clientY);
    } else if (touch.mode === "swipe") {
      event.preventDefault();
      const width = touch.row.getBoundingClientRect().width || 1;
      touch.row.style.transform = `translateX(${dx}px)`;
      touch.row.style.opacity = String(Math.max(0.2, 1 - Math.abs(dx) / width));
      touch.dx = dx;
    }
  };

  const touchEnd = () => {
    if (!touch) return;
    clearTimeout(touch.timer);
    const { row, mode, dx = 0 } = touch;
    touch = null;

    if (mode === "drag" && drag) {
      endDrag();
      swallowClick = true;
      setTimeout(() => { swallowClick = false; }, 300);
    } else if (mode === "swipe") {
      swallowClick = true;
      setTimeout(() => { swallowClick = false; }, 300);
      const width = row.getBoundingClientRect().width || 1;
      if (Math.abs(dx) > width * 0.35) {
        const index = rowsOf(row.parentElement).indexOf(row);
        row.style.transition = "transform 140ms ease, opacity 140ms ease";
        row.style.transform = `translateX(${dx > 0 ? width : -width}px)`;
        row.style.opacity = "0";
        setTimeout(() => {
          resetSwipe(row);
          if (index >= 0) send({ type: "remove", index });
        }, 140);
      } else {
        row.style.transition = "transform 140ms ease, opacity 140ms ease";
        row.style.transform = "";
        row.style.opacity = "";
        setTimeout(() => resetSwipe(row), 160);
      }
    }
  };
  document.addEventListener("touchend", touchEnd, { passive: true });
  document.addEventListener("touchcancel", touchEnd, { passive: true });

  // --------------------------------------------------------------- handle
  //
  // The phone drawer's grab bar. Dragged down far enough, or tapped, it
  // closes the drawer; the drawer follows the finger in between.

  let handle = null;

  document.addEventListener(
    "touchstart",
    (event) => {
      const grip = event.target.closest && event.target.closest(".drawer-handle");
      if (!grip) return;
      const drawer = grip.closest(".queue-drawer");
      if (!drawer) return;
      handle = { drawer, y: event.touches[0].clientY, dy: 0 };
      drawer.style.transition = "none";
    },
    { passive: true }
  );

  const handleMove = (event) => {
    if (!handle) return;
    event.preventDefault();
    handle.dy = Math.max(0, event.touches[0].clientY - handle.y);
    handle.drawer.style.transform = `translateY(${handle.dy}px)`;
  };

  const handleEnd = () => {
    if (!handle) return;
    const { drawer, dy } = handle;
    handle = null;
    drawer.style.transition = "";
    drawer.style.transform = "";
    if (dy > 60 || dy < 4) send({ type: "close" });
  };
  document.addEventListener("touchend", handleEnd, { passive: true });
  document.addEventListener("touchcancel", handleEnd, { passive: true });

  // A mouse clicks the handle to close, same as a tap.
  document.addEventListener("click", (event) => {
    if (event.target.closest && event.target.closest(".drawer-handle")) send({ type: "close" });
  });

  // ------------------------------------------------------------ touchmove
  //
  // The one listener that can cancel a scroll, so the one that makes the
  // browser hold every scroll it covers until this thread has answered. On
  // the document that was every list in the app; on the drawer it is only
  // the queue, the one list a finger can drag in. It cannot instead be added
  // when a press starts: the WebView settles whether a gesture's moves wait
  // for this thread when it begins, before any listener here has run.
  //
  // The drawer stays mounted while the queue view is, but a new view brings
  // a new drawer, so binding is retried until one is found and redone once
  // the bound one has left the page.

  let bound = null;

  const onTouchMove = (event) => {
    handleMove(event);
    rowMove(event);
  };

  window.stellyQueueBind = () => {
    if (bound && bound.isConnected) return true;
    const drawer = document.querySelector(".queue-drawer");
    if (bound) bound.removeEventListener("touchmove", onTouchMove);
    bound = drawer;
    if (drawer) drawer.addEventListener("touchmove", onTouchMove, { passive: false });
    return !!drawer;
  };

  // Late is better than never: a drawer that appeared unnoticed is bound by
  // the first touch anywhere, and drags from the next one on.
  document.addEventListener("touchstart", () => window.stellyQueueBind(), {
    passive: true,
    capture: true,
  });
}

// Run on each evaluation, so a remounted queue view rebinds its new drawer.
// Its first render may not have reached the page yet: try again each frame
// for a little while.
{
  let tries = 60;
  const bind = () => {
    if (!window.stellyQueueBind() && --tries > 0) requestAnimationFrame(bind);
  };
  bind();
}
