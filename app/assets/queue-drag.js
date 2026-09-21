// Drag-to-reorder for the queue.
//
// Pointer events rather than the HTML5 drag API: that API never fires on a
// touch screen, and this list has to work on a phone. One grip per row, with
// `touch-action: none` on the grip alone, so dragging the handle reorders and
// dragging anywhere else still scrolls the list.
//
// This moves nothing. It translates rows to show where the dragged one would
// land, then reports `{from, to}` and clears every transform, Rust owns the
// queue, and Dioxus re-renders the rows in the new order. Reordering the DOM
// here would be undone by the next render, and fight the diff besides.

const LIST = ".queue-list";
const GRIP = ".queue-grip";

// The channel belongs to this eval; the listeners outlive it. Keeping the
// sender on `window` lets a re-run swap in a fresh channel without stacking a
// second set of listeners on the document.
window.twoKhzQueueSend = (message) => dioxus.send(message);

if (!window.twoKhzQueueDragInstalled) {
  window.twoKhzQueueDragInstalled = true;

  let drag = null;

  const rowsOf = (list) =>
    Array.from(list.children).filter((node) => node.tagName === "LI");

  function begin(event) {
    // Primary button or a touch; a right-click on the grip should still open
    // the row's menu.
    if (event.button !== 0) return;

    const grip = event.target.closest && event.target.closest(GRIP);
    if (!grip) return;

    const list = grip.closest(LIST);
    const row = grip.closest("li");
    if (!list || !row) return;

    const items = rowsOf(list);
    const from = items.indexOf(row);
    if (from < 0) return;

    event.preventDefault();

    drag = {
      row,
      from,
      to: from,
      pointerId: event.pointerId,
      startY: event.clientY,
      items,
      // Measured once. Re-measuring per move would read rows this drag has
      // already translated, and the gap would drift.
      heights: items.map((node) => node.getBoundingClientRect().height),
    };

    try {
      grip.setPointerCapture(event.pointerId);
    } catch {
      // Not fatal: without capture the document listeners still see the move,
      // the drag just ends early if the pointer leaves the window.
    }
    row.classList.add("dragging");
  }

  function move(event) {
    if (!drag || event.pointerId !== drag.pointerId) return;
    event.preventDefault();

    const offset = event.clientY - drag.startY;
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

    // Open the gap: every row the dragged one has passed slides one place the
    // other way, by the dragged row's own height.
    const gap = drag.heights[drag.from];
    drag.items.forEach((node, i) => {
      if (node === drag.row) return;
      let shift = 0;
      if (to > drag.from && i > drag.from && i <= to) shift = -gap;
      if (to < drag.from && i >= to && i < drag.from) shift = gap;
      node.style.transform = shift ? `translateY(${shift}px)` : "";
    });
  }

  function end(event) {
    if (!drag || event.pointerId !== drag.pointerId) return;

    const { from, to, items, row } = drag;
    drag = null;

    // Clear before reporting: Rust re-renders these rows in the new order, and
    // a stale transform would offset whatever node lands in that slot.
    items.forEach((node) => {
      node.style.transform = "";
    });
    row.classList.remove("dragging");

    if (from !== to) window.twoKhzQueueSend({ type: "move", from, to });
  }

  // Capture phase, so a row's own click handler cannot swallow the gesture.
  document.addEventListener("pointerdown", begin, true);
  document.addEventListener("pointermove", move, true);
  document.addEventListener("pointerup", end, true);
  document.addEventListener("pointercancel", end, true);
}
