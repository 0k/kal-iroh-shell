import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  BUSY_CHECK_MS, BUSY_RENEW_MS, __resetShellReloadForTests, onShellReloadSave, prepareReload,
  registerShellBusy, restoreViewState, saveViewState,
} from '../src/reload';

/** Every answer the page sent to the shell. */
function acks(fetchMock: ReturnType<typeof vi.fn>): unknown[] {
  return fetchMock.mock.calls
    .filter(([url]) => url === '/__shell/reload-ack')
    .map(([, init]) => JSON.parse((init as RequestInit).body as string));
}

let fetchMock: ReturnType<typeof vi.fn>;
beforeEach(() => {
  vi.useFakeTimers();
  fetchMock = vi.fn(async () => new Response(null, { status: 204 }));
  vi.stubGlobal('fetch', fetchMock);
});
afterEach(() => {
  __resetShellReloadForTests();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  document.body.innerHTML = '';
});

describe('shell reload handshake (page side)', () => {
  it('saves everything then answers ready when nothing is in progress', () => {
    const order: string[] = [];
    onShellReloadSave(() => order.push('draft'));
    onShellReloadSave(() => { throw new Error('a broken saver'); });
    onShellReloadSave(() => order.push('scroll'));
    vi.spyOn(console, 'error').mockImplementation(() => undefined);

    prepareReload(3);

    expect(order).toEqual(['draft', 'scroll']);
    expect(acks(fetchMock)).toEqual([{ request: 3, status: 'ready' }]);
  });

  it('answers busy with the reason, renews it, and turns ready once it clears', () => {
    let recording = true;
    let saves = 0;
    registerShellBusy(() => (recording ? 'microphone on' : null));
    onShellReloadSave(() => { saves++; });

    prepareReload(1);
    expect(acks(fetchMock)).toEqual([{ request: 1, status: 'busy', reason: 'microphone on' }]);

    vi.advanceTimersByTime(BUSY_RENEW_MS);
    expect(acks(fetchMock)).toHaveLength(2);

    recording = false;
    vi.advanceTimersByTime(BUSY_CHECK_MS);
    expect(acks(fetchMock).at(-1)).toEqual({ request: 1, status: 'ready' });
    expect(saves).toBe(2);

    vi.advanceTimersByTime(BUSY_RENEW_MS * 2);
    expect(acks(fetchMock)).toHaveLength(3);
  });

  it('puts the cursor back in the field being typed in after the reload', () => {
    const page = '<main><div><textarea></textarea></div></main>';
    document.body.innerHTML = page;
    const before = document.querySelector('textarea')!;
    before.value = 'hello world';
    before.focus();
    before.setSelectionRange(6, 11);

    saveViewState();
    document.body.innerHTML = page;
    document.querySelector('textarea')!.value = 'hello world';
    restoreViewState();

    const after = document.querySelector('textarea')!;
    expect(document.activeElement).toBe(after);
    expect([after.selectionStart, after.selectionEnd]).toEqual([6, 11]);
  });
});
