/**
 * Page side of the shell's reload handshake.
 *
 * When a new release of the interface is published, the client shell
 * calls `window.__kalIrohShell.prepareReload(request)`.  The page then:
 *
 * 1. runs every registered SAVER (drafts, transcript position, focused
 *    field and caret…), so nothing typed or viewed is lost;
 * 2. answers `POST /__shell/reload-ack`:
 *    - `ready` when nothing holds it back — the shell reloads at once;
 *    - `busy` + reason while a BUSY PROBE reports the user in the middle
 *      of something (microphone on, upload, message being sent).  The
 *      answer is renewed every {@link BUSY_RENEW_MS} (the shell gives
 *      each one a 30 s lease) and turns into `ready`, after saving
 *      again, as soon as every probe is clear.
 *
 * A page that never answers is reloaded by the shell after 5 s.
 *
 * Composables register what they own with {@link useShellReloadSave} /
 * {@link useShellBusy}; the registration lasts for their scope.
 */
import { onScopeDispose } from 'vue';
import { PAGE_GLOBAL, RELOAD_ACK_PATH, type ReloadAck } from './protocol';

/** A probe returns the reason the reload must wait, or null. */
export type BusyProbe = () => string | null;

export const BUSY_RENEW_MS = 10_000;
export const BUSY_CHECK_MS = 500;
/** Where the focused field and caret wait across the reload. */
export const VIEW_STATE_KEY = 'kal-iroh-shell.reload-view';
/** How long a restored page keeps looking for the field to refocus. */
export const RESTORE_FOCUS_MS = 3_000;

const savers = new Set<() => void>();
const probes = new Set<BusyProbe>();

export function onShellReloadSave(save: () => void): () => void {
  savers.add(save);
  return () => { savers.delete(save); };
}

export function registerShellBusy(probe: BusyProbe): () => void {
  probes.add(probe);
  return () => { probes.delete(probe); };
}

/** Run `save` before a shell reload, for the current scope's lifetime. */
export function useShellReloadSave(save: () => void): void {
  onScopeDispose(onShellReloadSave(save));
}

/** Postpone a shell reload with `reason` while `active()` holds. */
export function useShellBusy(reason: string, active: () => boolean): void {
  onScopeDispose(registerShellBusy(() => (active() ? reason : null)));
}

function runSavers(): void {
  for (const save of savers) {
    try {
      save();
    } catch (e) {
      console.error('shell reload: a saver failed', e);
    }
  }
}

function busyReasons(): string[] {
  const reasons = new Set<string>();
  for (const probe of probes) {
    const reason = probe();
    if (reason) reasons.add(reason);
  }
  return [...reasons];
}

function sendAck(ack: ReloadAck): void {
  void fetch(RELOAD_ACK_PATH, {
    method: 'POST',
    keepalive: true,
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(ack),
  }).catch(e => console.warn('shell reload: answer failed', e));
}

let active: { request: number; check: ReturnType<typeof setInterval> } | null = null;

/** Handle the shell's "prepare to reload `request`". */
export function prepareReload(request: number): void {
  if (active) clearInterval(active.check);
  active = null;
  runSavers();
  let reasons = busyReasons();
  if (reasons.length === 0) {
    sendAck({ request, status: 'ready' });
    return;
  }
  sendAck({ request, status: 'busy', reason: reasons.join(', ') });
  let lastBusyAck = Date.now();
  const check = setInterval(() => {
    reasons = busyReasons();
    if (reasons.length === 0) {
      clearInterval(check);
      active = null;
      runSavers();
      sendAck({ request, status: 'ready' });
    } else if (Date.now() - lastBusyAck >= BUSY_RENEW_MS) {
      lastBusyAck = Date.now();
      sendAck({ request, status: 'busy', reason: reasons.join(', ') });
    }
  }, BUSY_CHECK_MS);
  active = { request, check };
}

/** CSS path of `el` from `body`: its id when it has one, else positions. */
function cssPath(el: Element): string {
  const parts: string[] = [];
  let node: Element | null = el;
  while (node && node !== document.body) {
    if (node.id) {
      parts.unshift(`#${CSS.escape(node.id)}`);
      break;
    }
    const parent: Element | null = node.parentElement;
    const index = parent ? Array.prototype.indexOf.call(parent.children, node) + 1 : 1;
    parts.unshift(`${node.tagName.toLowerCase()}:nth-child(${index})`);
    node = parent;
  }
  return parts.join(' > ');
}

interface ViewState {
  focus: { path: string; start: number | null; end: number | null } | null;
}

/** Saver: remember the focused field and its caret / selection. */
export function saveViewState(): void {
  const el = document.activeElement;
  let focus: ViewState['focus'] = null;
  if (el && el !== document.body && el instanceof HTMLElement) {
    const text = el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement ? el : null;
    let start: number | null = null;
    let end: number | null = null;
    try {
      start = text?.selectionStart ?? null;
      end = text?.selectionEnd ?? null;
    } catch {
      // Inputs without a selection (checkbox…) throw; no caret to keep.
    }
    focus = { path: cssPath(el), start, end };
  }
  sessionStorage.setItem(VIEW_STATE_KEY, JSON.stringify({ focus } satisfies ViewState));
}

/** After a reload: refocus the saved field once it exists. */
export function restoreViewState(): void {
  const raw = sessionStorage.getItem(VIEW_STATE_KEY);
  if (!raw) return;
  sessionStorage.removeItem(VIEW_STATE_KEY);
  const { focus } = JSON.parse(raw) as ViewState;
  if (!focus) return;
  const deadline = Date.now() + RESTORE_FOCUS_MS;
  const attempt = (): void => {
    const el = document.querySelector(focus.path);
    if (el instanceof HTMLElement) {
      el.focus();
      if ((el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement) && focus.start !== null) {
        try {
          el.setSelectionRange(focus.start, focus.end ?? focus.start);
        } catch {
          // Not a text field in this release.
        }
      }
      return;
    }
    if (Date.now() < deadline) setTimeout(attempt, 100);
  };
  attempt();
}

/** Expose the handshake to the shell; the view state is always saved. */
export function installShellReload(): void {
  onShellReloadSave(saveViewState);
  (window as unknown as Record<string, unknown>)[PAGE_GLOBAL] = { prepareReload };
}

export function __resetShellReloadForTests(): void {
  if (active) clearInterval(active.check);
  active = null;
  savers.clear();
  probes.clear();
}
