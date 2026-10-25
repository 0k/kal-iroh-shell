/**
 * Cold-start timing marks.
 *
 * Each phase is reported once per page load to the shell's local
 * `POST /__shell/boot-mark`, which logs it against the shell PROCESS
 * start: the cold-start figure (process start → phase), comparable
 * between the embedded and the remote interface.  Sent as a beacon: it
 * never competes with the page's own requests, and a page served by
 * something other than the shell merely gets a 404.  Without
 * `sendBeacon` (jsdom) nothing is sent.
 */
import { BOOT_MARK_PATH, UI_BUILD_ATTR, UI_SOURCE_ATTR } from './protocol';

const sent = new Set<string>();

function rootAttr(name: string): string {
  return document.querySelector(`[${name}]`)?.getAttribute(name) ?? 'unknown';
}

/** Which interface the shell served this page: `remote` / `embedded`. */
export function uiSource(): string {
  return rootAttr(UI_SOURCE_ATTR);
}

/** The build id of this page, as the shell reported it. */
export function uiBuild(): string {
  return rootAttr(UI_BUILD_ATTR);
}

/** Report that the page reached `phase` (once per page load). */
export function markBoot(phase: string): void {
  if (sent.has(phase)) return;
  sent.add(phase);
  const body = JSON.stringify({
    phase,
    epoch_ms: Date.now(),
    page_ms: Math.round(performance.now()),
    source: uiSource(),
    build_id: uiBuild(),
  });
  navigator.sendBeacon?.(BOOT_MARK_PATH, new Blob([body], { type: 'application/json' }));
}
