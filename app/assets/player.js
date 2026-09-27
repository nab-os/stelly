// Playback transport.
//
// Owns the <audio> element, so seeking and progress never round-trip through
// Rust. Only track-boundary events come back.

(async () => {
  // Scheduled from the render that mounts the element, so poll briefly rather
  // than assuming the DOM has flushed.
  let audio = document.getElementById("player");
  for (let attempt = 0; attempt < 60 && !audio; attempt++) {
    await new Promise((resolve) => setTimeout(resolve, 50));
    audio = document.getElementById("player");
  }
  if (!audio) return;

  // `start` is where the session is by now, which is not always the top: a
  // device taking over the sound picks up mid-track. Says "loaded" once the
  // element has started or given up, so Rust can tell the pause a src swap
  // causes from someone pausing.
  window.twoKhzPlayUrl = (url, start, autoplay) => {
    audio.src = url;
    // Set even when it is 0: a previous track's position survives a src swap
    // in some webviews.
    audio.currentTime = start || 0;
    const loaded = () => dioxus.send({ type: "loaded" });
    if (autoplay) {
      audio.play().then(loaded, loaded);
    } else {
      setTimeout(loaded, 0);
    }
  };
  window.twoKhzResume = () => audio.play().catch(() => {});
  window.twoKhzPause = () => audio.pause();
  window.twoKhzStop = () => {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  };
  window.twoKhzSeekTo = (seconds) => {
    audio.currentTime = Math.max(0, seconds);
  };
  window.twoKhzVolume = (value) => {
    audio.volume = Math.max(0, Math.min(1, value));
  };

  // What the OS shows as "now playing", and where a hardware or mouse
  // previous/next button (and a lock screen's transport row) actually comes
  // from, without this, WebKitGTK has nothing to hand to MPRIS/PipeWire's
  // media-session bridge, so the app's name and art never reach it and its
  // buttons have nothing registered to call.
  window.twoKhzSetMetadata = (title, artist, album, artwork) => {
    if (!("mediaSession" in navigator)) return;
    navigator.mediaSession.metadata = new MediaMetadata({
      title,
      artist,
      album,
      artwork: artwork ? [{ src: artwork, sizes: "512x512", type: "image/jpeg" }] : [],
    });
  };

  if ("mediaSession" in navigator) {
    // Play/pause stay local, the audio element's own events below already
    // tell Rust when they fire, the same as a click on the transport bar.
    navigator.mediaSession.setActionHandler("play", () => audio.play().catch(() => {}));
    navigator.mediaSession.setActionHandler("pause", () => audio.pause());
    // Previous/next change *which track*, which only Rust knows how to pick
    // (the queue, not this element), so these cross back over the channel
    // rather than touching `audio` directly.
    navigator.mediaSession.setActionHandler("previoustrack", () => {
      dioxus.send({ type: "transport", action: "previous" });
    });
    navigator.mediaSession.setActionHandler("nexttrack", () => {
      dioxus.send({ type: "transport", action: "next" });
    });
  }

  audio.addEventListener("ended", () => dioxus.send({ type: "ended" }));
  audio.addEventListener("play", () => {
    if ("mediaSession" in navigator) navigator.mediaSession.playbackState = "playing";
    dioxus.send({ type: "playing", playing: true, position: audio.currentTime || 0 });
  });
  audio.addEventListener("pause", () => {
    if ("mediaSession" in navigator) navigator.mediaSession.playbackState = "paused";
    dioxus.send({ type: "playing", playing: false, position: audio.currentTime || 0 });
  });

  audio.addEventListener("error", () => {
    // Signed URLs expire, so a late click on a stale queue entry lands here
    // rather than on the Qobuz error path.
    dioxus.send({ type: "failed" });
  });

  // timeupdate fires about four times a second; twice is plenty for a progress
  // bar and leaves room on the channel for the map's selection messages.
  let lastSent = 0;
  audio.addEventListener("timeupdate", () => {
    const now = performance.now();
    if (now - lastSent < 500) return;
    lastSent = now;
    dioxus.send({
      type: "time",
      position: audio.currentTime || 0,
      duration: Number.isFinite(audio.duration) ? audio.duration : 0,
    });
  });

  // Keep the channel open so Rust keeps receiving transport events.
  while (true) {
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
})();
