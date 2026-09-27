// Android's back key, and the mouse's back and forward buttons.
//
// The app never navigates the webview, so its session history is one page
// long. Android's back key asks the webview to go back and, when it can't,
// finishes the activity: the key closed the app from any page. So while the
// app has somewhere to go back to, Rust arms this, and one extra entry sits
// on the webview's history for the key to pop. The `popstate` that follows
// is the key press, and goes to Rust as a "back". Disarmed, the entry is
// taken off again and the key closes the app as before.
//
// The mouse's side buttons never touch the history: they go straight to Rust,
// and their default is cancelled so a Chromium-based webview does not also
// navigate on its own.

window.twoKhzBackSend = (message) => dioxus.send(message);

if (!window.twoKhzBackInstalled) {
  window.twoKhzBackInstalled = true;

  // On a phone the generate panel covers the main area, and back should
  // close it; on a wide screen it is docked and there is nothing to close.
  const narrow = window.matchMedia("(max-width: 900px)");
  const send = (type) =>
    window.twoKhzBackSend && window.twoKhzBackSend({ type, narrow: narrow.matches });

  // Whether the extra entry is on the history and current.
  let armed = false;
  // Set while taking the entry off, so that `popstate` is not a key press.
  let dropping = false;

  window.twoKhzBackArm = (want) => {
    if (want && !armed) {
      history.pushState({ twoKhz: true }, "");
      armed = true;
    } else if (!want && armed) {
      armed = false;
      dropping = true;
      history.back();
    }
  };

  window.addEventListener("popstate", () => {
    if (dropping) {
      dropping = false;
      return;
    }
    armed = false;
    send("back");
  });

  const sideButton = (event) =>
    event.button === 3 ? "back" : event.button === 4 ? "forward" : null;

  window.addEventListener("mousedown", (event) => {
    if (sideButton(event)) event.preventDefault();
  });
  window.addEventListener("mouseup", (event) => {
    const type = sideButton(event);
    if (!type) return;
    event.preventDefault();
    send(type);
  });
}
