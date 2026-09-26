// Long press opens the row menu on a touch screen.
//
// Right-click covers the pointer case and is handled in Rust, per row. Touch
// has no right-click, which left every row action unreachable on a phone,
// the menu existed and nothing could open it.
//
// `oncontextmenu` does fire on a long press in WebKitGTK, but only after the
// platform's own selection UI has engaged, and Android's WebView swallows it
// often enough not to rely on. So the gesture is detected here.
//
// Delegated from the document and installed once: rows come and go constantly,
// and a listener per row would be thousands of them. The send function is
// swappable so a re-run replaces the channel instead of stacking a second
// copy of every listener.
(() => {
  const HOLD_MS = 500;
  // Enough slack for a thumb that is not perfectly still, tight enough that a
  // deliberate scroll cancels.
  const SLOP_PX = 10;

  // Rebound on every run, before the install guard: `dioxus` here is *this*
  // eval's channel, so a re-evaluation has to replace the send target. Doing
  // it after the guard would leave the listeners posting into a closed one.
  window.twoKhzLongPressSend = (message) => dioxus.send(message);
  window.twoKhzCloseMap = () => dioxus.send({ closeMap: true });

  if (window.twoKhzLongPressInstalled) return;
  window.twoKhzLongPressInstalled = true;

  let timer = null;
  let origin = null;
  // Set when the menu opens, so the click that ends the press does not also
  // activate the row underneath it.
  let swallowNextClick = false;

  const cancel = () => {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    origin = null;
  };

  document.addEventListener(
    "pointerdown",
    (event) => {
      // A mouse keeps the native path: right-click is instant, and a mouse
      // held still over a row should not sprout a menu.
      if (event.pointerType === "mouse") return;

      const row = event.target.closest && event.target.closest("[data-menu]");
      if (!row) return;

      origin = { x: event.clientX, y: event.clientY };
      timer = setTimeout(() => {
        timer = null;
        swallowNextClick = true;
        if (window.twoKhzLongPressSend) {
          window.twoKhzLongPressSend({
            target: row.dataset.menu,
            x: origin ? origin.x : 0,
            y: origin ? origin.y : 0,
          });
        }
        origin = null;
      }, HOLD_MS);
    },
    { passive: true }
  );

  document.addEventListener(
    "pointermove",
    (event) => {
      if (timer === null || !origin) return;
      const dx = Math.abs(event.clientX - origin.x);
      const dy = Math.abs(event.clientY - origin.y);
      if (dx > SLOP_PX || dy > SLOP_PX) cancel();
    },
    { passive: true }
  );

  document.addEventListener("pointerup", cancel, { passive: true });
  document.addEventListener("pointercancel", cancel, { passive: true });
  // Capture: the scroll that cancels a press happens inside the list, and a
  // scroll event on a descendant does not bubble.
  document.addEventListener("scroll", cancel, { capture: true, passive: true });

  document.addEventListener(
    "click",
    (event) => {
      if (!swallowNextClick) return;
      swallowNextClick = false;
      event.stopPropagation();
      event.preventDefault();
    },
    { capture: true }
  );

  // Escape closes the map overlay. Here rather than in its own eval because
  // this is already the install-once keyboard channel.
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && window.twoKhzCloseMap) window.twoKhzCloseMap();
  });
})();
