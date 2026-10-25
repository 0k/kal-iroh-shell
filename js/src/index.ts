/**
 * kal-iroh-shell page kit.
 *
 * Call {@link installShellReload} once at startup and {@link restoreViewState}
 * after the app mounts.  Components register what they own with
 * {@link useShellReloadSave} / {@link useShellBusy}.
 */
export * from './protocol';
export {
  BUSY_CHECK_MS, BUSY_RENEW_MS, RESTORE_FOCUS_MS, VIEW_STATE_KEY,
  installShellReload, onShellReloadSave, prepareReload, registerShellBusy, restoreViewState,
  saveViewState, useShellBusy, useShellReloadSave, __resetShellReloadForTests,
} from './reload';
export type { BusyProbe } from './reload';
export { markBoot, uiBuild, uiSource } from './bootMark';
