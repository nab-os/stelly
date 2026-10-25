// Rendering while nothing is on screen.
//
// Dioxus hands each render to the webview, which applies it on the next
// animation frame and then says so. Until it has, Rust polls none of the
// app's tasks: not the session stream, not the notification's buttons, not
// the element's "ended" or a media key. A webview off screen draws no frames,
// Android's once the app is in the background or the phone locked, WebKitGTK's
// once the window is hidden, so the first render after that held everything
// back until the app was in front again.
//
// So a render that arrives while the page is hidden is applied at once, as is
// one still waiting for its frame when the page goes, and none waits longer
// than GRACE for a frame that may never come.
(async () => {
  // How long a render waits for its frame. Applied without one it is only
  // drawn on the next, which is all a frame would have done.
  const GRACE = 200;

  let interpreter = window.interpreter;
  for (let attempt = 0; attempt < 60 && !interpreter; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 50));
    interpreter = window.interpreter;
  }
  if (!interpreter || interpreter.stellyOffscreen) return;
  interpreter.stellyOffscreen = true;

  // The render waiting for its frame, if any. Rust sends the next only once
  // this one is done, so there is never more than one.
  let waiting = null;
  interpreter.rafEdits = (bytes) => {
    interpreter.enqueueBytes(bytes);
    let done = false;
    const apply = () => {
      if (done) return;
      done = true;
      waiting = null;
      interpreter.flushQueuedBytes();
      interpreter.markEditsFinished();
    };
    if (document.hidden) {
      apply();
      return;
    }
    waiting = apply;
    requestAnimationFrame(apply);
    setTimeout(apply, GRACE);
  };
  document.addEventListener("visibilitychange", () => {
    if (document.hidden && waiting) waiting();
  });
})();
