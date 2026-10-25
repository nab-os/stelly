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
  //
  // Taking over mid-track, the session's clock runs on while the element
  // fetches enough to start, a second or so from the CDN. So it waits until
  // it can play and only then moves up to where the session is by now, rather
  // than starting where it was when asked and sounding behind.
  let swaps = 0;
  window.stellyPlayUrl = (url, start, autoplay) => {
    const swap = ++swaps;
    const asked = performance.now();
    audio.src = url;
    // Set even when it is 0: a previous track's position survives a src swap
    // in some webviews.
    audio.currentTime = start || 0;
    const loaded = () => dioxus.send({ type: "loaded" });
    if (!autoplay) {
      setTimeout(loaded, 0);
    } else if (start > 0) {
      audio.addEventListener(
        "canplay",
        () => {
          if (swap !== swaps) return;
          let now = start + (performance.now() - asked) / 1000;
          if (Number.isFinite(audio.duration)) now = Math.min(now, audio.duration);
          audio.currentTime = now;
          audio.play().then(loaded, loaded);
        },
        { once: true },
      );
    } else {
      audio.play().then(loaded, loaded);
    }
  };
  window.stellyResume = () => audio.play().catch(() => {});
  window.stellyPause = () => audio.pause();
  window.stellyStop = () => {
    audio.pause();
    audio.removeAttribute("src");
    audio.load();
  };
  window.stellySeekTo = (seconds) => {
    audio.currentTime = Math.max(0, seconds);
  };
  window.stellyVolume = (value) => {
    audio.volume = Math.max(0, Math.min(1, value));
  };
  // Whatever moves it, this page or the system's mixer, which WebKitGTK
  // passes on to the element. Once now, for the slider's starting point.
  const volume = () => dioxus.send({ type: "volume", level: audio.volume });
  audio.addEventListener("volumechange", volume);
  volume();

  // What the OS shows as "now playing", and where a hardware or mouse
  // previous/next button (and a lock screen's transport row) actually comes
  // from, without this, WebKitGTK has nothing to hand to MPRIS/PipeWire's
  // media-session bridge, so the app's name and art never reach it and its
  // buttons have nothing registered to call.
  window.stellySetMetadata = (title, artist, album, artwork) => {
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
    // A seek moves the session's position, which every device follows, so
    // it goes through Rust like the seek line's. WebKitGTK passes MPRIS's
    // relative Seek on as an absolute time, which nothing here can undo.
    navigator.mediaSession.setActionHandler("seekto", (details) => {
      if (!Number.isFinite(details.seekTime)) return;
      dioxus.send({ type: "transport", action: "seek", position: Math.max(0, details.seekTime) });
    });
  }

  audio.addEventListener("ended", () => dioxus.send({ type: "ended" }));
  audio.addEventListener("play", () => {
    if ("mediaSession" in navigator) navigator.mediaSession.playbackState = "playing";
    dioxus.send({ type: "playing", playing: true, position: audio.currentTime || 0 });
  });
  // `play` is the element being asked, `playing` the sound starting. Only
  // timed, see src/timings.rs.
  if (window.stellyTimings) {
    audio.addEventListener("playing", () => dioxus.send({ type: "heard" }));
  }
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
