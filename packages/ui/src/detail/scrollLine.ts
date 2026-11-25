// Scrolling a transcript line into the middle of the pane (T14.18). Lines off screen are not
// laid out (`content-visibility: auto`) and stand in at an estimated height, so the first scroll
// to a far line lands where the estimate puts it; the lines drawn on the way then take their
// real heights and move it. The scroll is repeated on the next frames until the line stops
// moving (at most `SETTLE_FRAMES`), so it ends in view after layout settles.

/** The most frames a scroll is repeated for. */
export const SETTLE_FRAMES = 8;

/** Scrolls `el` to the middle of the pane and keeps it there while layout settles; returns a
 * function that stops it. */
export function scrollLineIntoView(el: HTMLElement): () => void {
  let frame = 0;
  let handle: number | null = null;
  let last = Number.NaN;
  const scroll = () => {
    el.scrollIntoView?.({ block: "center" });
    last = el.getBoundingClientRect().top;
  };
  const check = () => {
    handle = null;
    frame += 1;
    const top = el.getBoundingClientRect().top;
    if (!el.isConnected || top === last || frame > SETTLE_FRAMES) return;
    scroll();
    handle = requestAnimationFrame(check);
  };
  scroll();
  if (typeof requestAnimationFrame === "function") handle = requestAnimationFrame(check);
  return () => {
    if (handle !== null) cancelAnimationFrame(handle);
    handle = null;
  };
}
