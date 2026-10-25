/**
 * Wire names of kal-iroh-shell, as seen from the page.
 *
 * Twins of the Rust crate `kal-iroh-shell-protocol`; `test/protocol.test.ts`
 * fails when the two disagree.
 */

/** `POST`: the page's answer to "prepare to reload". */
export const RELOAD_ACK_PATH = '/__shell/reload-ack';
/** `GET` / `POST`: boot timing marks. */
export const BOOT_MARK_PATH = '/__shell/boot-mark';
/** `GET`: setting, served release, current release, reload state. */
export const STATUS_PATH = '/__shell/status';
/** Attribute on the page's root element: `remote` or `embedded`. */
export const UI_SOURCE_ATTR = 'data-ui-source';
/** Attribute on the page's root element: the running build id. */
export const UI_BUILD_ATTR = 'data-ui-build';
/** Global object the shell calls: `window.__kalIrohShell.prepareReload(n)`. */
export const PAGE_GLOBAL = '__kalIrohShell';
/** Publish directory: file naming the current release. */
export const POINTER_FILE = 'current.json';
/** Release directory: file listing its files. */
export const MANIFEST_FILE = 'ui-manifest.json';

export type ReloadAck =
  | { request: number; status: 'ready' }
  | { request: number; status: 'busy'; reason: string };
