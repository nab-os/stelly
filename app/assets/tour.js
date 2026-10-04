// The tour's ring and card, placed against whatever the step points at.
//
// Defined once and then only called: every step evals this file again with a
// new list of selectors. The first of them on screen gets the ring, a hole in
// a shade over everything else; none, and the ring shrinks to a point in the
// middle, its shade then covering the whole window. The card goes on the
// side of the ring with room for it, below, above, left or right, in that
// order, or in the middle if none has.
(() => {
  if (window.stellyTour) return;

  let wanted = [];

  const onScreen = (el) => {
    if (!el || getComputedStyle(el).visibility === 'hidden') return null;
    const r = el.getBoundingClientRect();
    const visible = r.width > 0 && r.height > 0 && r.right > 0 && r.bottom > 0
      && r.left < innerWidth && r.top < innerHeight;
    return visible ? r : null;
  };

  const place = () => {
    const tour = document.querySelector('.tour');
    const ring = document.querySelector('.tour-ring');
    const card = document.querySelector('.tour-card');
    if (!tour || !ring || !card) return;

    let r = null;
    for (const selector of wanted) {
      r = onScreen(document.querySelector(selector));
      if (r) break;
    }

    const vw = innerWidth;
    const vh = innerHeight;
    const pad = 6;
    const gap = 14;
    const edge = 12;
    const cw = card.offsetWidth;
    const ch = card.offsetHeight;
    const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
    let x = (vw - cw) / 2;
    let y = (vh - ch) / 2;

    if (r) {
      const top = Math.max(r.top - pad, 0);
      const left = Math.max(r.left - pad, 0);
      const right = Math.min(r.right + pad, vw);
      const bottom = Math.min(r.bottom + pad, vh);
      Object.assign(ring.style, {
        top: `${top}px`, left: `${left}px`,
        width: `${right - left}px`, height: `${bottom - top}px`,
      });
      ring.classList.remove('none');

      const midX = (left + right) / 2 - cw / 2;
      const midY = (top + bottom) / 2 - ch / 2;
      if (vh - bottom >= ch + gap + edge) {
        [x, y] = [midX, bottom + gap];
      } else if (top >= ch + gap + edge) {
        [x, y] = [midX, top - gap - ch];
      } else if (left >= cw + gap + edge) {
        [x, y] = [left - gap - cw, midY];
      } else if (vw - right >= cw + gap + edge) {
        [x, y] = [right + gap, midY];
      }
    } else {
      Object.assign(ring.style, { top: `${vh / 2}px`, left: `${vw / 2}px`, width: '0px', height: '0px' });
      ring.classList.add('none');
    }

    card.style.left = `${clamp(x, edge, Math.max(edge, vw - cw - edge))}px`;
    card.style.top = `${clamp(y, edge, Math.max(edge, vh - ch - edge))}px`;
    card.classList.add('placed');
  };

  window.stellyTour = (selectors) => {
    wanted = selectors;
    place();
    // Again once anything the step's render moved has settled.
    requestAnimationFrame(place);
    setTimeout(place, 250);
    const tour = document.querySelector('.tour');
    if (tour && !tour.contains(document.activeElement)) tour.focus({ preventScroll: true });
  };

  addEventListener('resize', place);
})();
