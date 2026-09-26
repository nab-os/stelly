// Long press opens the row menu, on a touch screen and with the mouse.
//
// Right-click covers the pointer case and is handled in Rust, per row. Touch
// has no right-click, which left every row action unreachable on a phone,
// the menu existed and nothing could open it.
//
// `oncontextmenu` does fire on a long press in WebKitGTK, but only after the
// platform's own selection UI has engaged, and Android's WebView swallows it
// often enough not to rely on. So the gesture is detected here.
//
// WebKitGTK has the mirror-image bug on the mouse path: a right-click fires
// `contextmenu` (which Rust's `oncontextmenu` handles, opening the menu) and
// then *also* fires a plain `click` on the same row, targeting whatever was
// under the pointer when the button went down, which is the row, not the
// menu that has since opened on top of it. Rust's `onclick` for a row
// selects the track, which for a track opens the detail sheet over the
// menu that a moment ago opened correctly. From outside that reads as
// "right-click opens the sheet instead of a menu", because the menu is
// there for one frame and then immediately buried. The swallow-the-next-click
// trick below already existed for long press; a `contextmenu` listener
// arms the same flag for the mouse path.
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
      // A mouse gets it too, on the main button: right-click still works,
      // but holding a row is the same gesture everywhere, which is easier
      // to remember than which input device wants which.
      if (event.pointerType === "mouse" && event.button !== 0) return;

      const row = event.target.closest && event.target.closest("[data-menu]");
      if (!row) return;
      // A held grip is the start of a queue drag, not a request for a menu.
      if (event.target.closest(".queue-grip")) return;

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

  // Arms the swallow for the mouse path, see the comment at the top of the
  // file. Capture phase and no target check: whatever WebKit sends `click`
  // to next is the thing to drop, on a row or not.
  document.addEventListener(
    "contextmenu",
    () => {
      swallowNextClick = true;
      // In case WebKit does not follow this contextmenu with a click after
      // all (behaviour that motivated this fix has not been seen on every
      // platform build), so a flag armed here cannot outlive the gesture
      // that set it and swallow an unrelated later click.
      setTimeout(() => { swallowNextClick = false; }, 300);
    },
    { capture: true }
  );

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
