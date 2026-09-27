// Gestures for the queue drawer: reorder by dragging a row, remove one by
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
// queue and re-renders it in the new order.

window.twoKhzQueueSend = (message) => dioxus.send(message);

if (!window.twoKhzQueueGesturesInstalled) {
  window.twoKhzQueueGesturesInstalled = true;

  const HOLD_MS = 350;
  const SLOP_PX = 8;
  const MOUSE_SLOP_PX = 4;

  const send = (message) => window.twoKhzQueueSend && window.twoKhzQueueSend(message);

  const rowsOf = (list) =>
    Array.from(list.children).filter((node) => node.tagName === "LI");

  // Buttons and names inside a row do their own thing; a press on one of
  // them is never the start of a drag.
  const rowFor = (target) => {
    if (!target.closest || target.closest("button, .clickable")) return null;
    const row = target.closest(".queue-list > li");
    return row || null;
  };

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

  function startDrag(row, y) {
    const list = row.parentElement;
    const items = rowsOf(list);
    const from = items.indexOf(row);
    if (from < 0) return false;
    drag = {
      row,
      from,
      to: from,
      startY: y,
      items,
      // Measured once: re-measuring per move would read rows this drag has
      // already translated.
      heights: items.map((node) => node.getBoundingClientRect().height),
    };
    row.classList.add("dragging");
    if (navigator.vibrate) navigator.vibrate(12);
    return true;
  }

  function moveDrag(y) {
    const offset = y - drag.startY;
    drag.row.style.transform = `translateY(${offset}px)`;

    // Walk outward from the original slot, consuming neighbour heights, until
    // the pointer no longer clears the next row's midpoint.
    let to = drag.from;
    if (offset > 0) {
      let edge = 0;
      for (let i = drag.from + 1; i < drag.items.length; i++) {
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
    drag.items.forEach((node, i) => {
      if (node === drag.row) return;
      let shift = 0;
      if (to > drag.from && i > drag.from && i <= to) shift = -gap;
      if (to < drag.from && i >= to && i < drag.from) shift = gap;
      node.style.transform = shift ? `translateY(${shift}px)` : "";
    });
  }

  function endDrag() {
    const { from, to, items, row } = drag;
    drag = null;
    // Cleared before reporting: Rust re-renders these rows in the new order,
    // and a stale transform would offset whatever lands in that slot.
    items.forEach((node) => {
      node.style.transform = "";
    });
    row.classList.remove("dragging");
    if (from !== to) send({ type: "move", from, to });
  }

  // ---------------------------------------------------------------- mouse

  let mouse = null;

  document.addEventListener("pointerdown", (event) => {
    if (event.pointerType !== "mouse" || event.button !== 0) return;
    const row = rowFor(event.target);
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
      const row = rowFor(event.target);
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

  document.addEventListener(
    "touchmove",
    (event) => {
      if (!touch) return;
      const point = event.touches[0];
      const dx = point.clientX - touch.x;
      const dy = point.clientY - touch.y;

      if (touch.mode === "pending") {
        if (Math.abs(dx) < SLOP_PX && Math.abs(dy) < SLOP_PX) return;
        clearTimeout(touch.timer);
        if (Math.abs(dx) > Math.abs(dy)) {
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
    },
    { passive: false }
  );

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

  document.addEventListener(
    "touchmove",
    (event) => {
      if (!handle) return;
      event.preventDefault();
      handle.dy = Math.max(0, event.touches[0].clientY - handle.y);
      handle.drawer.style.transform = `translateY(${handle.dy}px)`;
    },
    { passive: false }
  );

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
}
