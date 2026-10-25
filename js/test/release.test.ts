// @vitest-environment node
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import {
  MANIFEST_FILE, POINTER_FILE, newBuildId, publishRelease, readPointer, writeManifest,
} from '../src/vite.mjs';

const dirs: string[] = [];
function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), 'ui-release-'));
  dirs.push(dir);
  return dir;
}
afterEach(() => { for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true }); });

/** A fake Vite output with an index and one asset, and its manifest. */
function fakeBuild(buildId: string, mainJs = 'console.log(1)'): string {
  const dist = tempDir();
  mkdirSync(join(dist, 'assets'));
  writeFileSync(join(dist, 'index.html'), '<div id="app"></div>');
  writeFileSync(join(dist, 'assets', 'main.js'), mainJs);
  writeManifest(dist, buildId);
  return dist;
}

describe('ui release publishing', () => {
  it('describes every built file by size and sha256', () => {
    const dist = fakeBuild('20261023T000000-aaaaaa', 'abc');
    const manifest = JSON.parse(readFileSync(join(dist, MANIFEST_FILE), 'utf8'));
    expect(manifest).toEqual({
      build_id: '20261023T000000-aaaaaa',
      files: [
        { path: 'assets/main.js', size: 3,
          sha256: 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad' },
        { path: 'index.html', size: 20, sha256: expect.stringMatching(/^[0-9a-f]{64}$/) },
      ],
    });
  });

  it('publishes a frozen copy and points current.json at it', () => {
    const publish = tempDir();
    const id = publishRelease(fakeBuild('20261023T000000-aaaaaa'), publish, new Date('2026-10-23T00:00:00Z'));

    expect(readFileSync(join(publish, id, 'assets', 'main.js'), 'utf8')).toBe('console.log(1)');
    expect(readPointer(publish)).toEqual({ build_id: id, published_at: '2026-10-23T00:00:00.000Z' });
    expect(readdirSync(publish).sort()).toEqual([POINTER_FILE, id].sort());
  });

  it('keeps an earlier release readable after a newer one becomes current', () => {
    const publish = tempDir();
    publishRelease(fakeBuild('20261023T000000-aaaaaa', 'old'), publish);
    publishRelease(fakeBuild('20261023T000001-bbbbbb', 'new'), publish);

    expect(readPointer(publish)?.build_id).toBe('20261023T000001-bbbbbb');
    expect(readFileSync(join(publish, '20261023T000000-aaaaaa', 'assets', 'main.js'), 'utf8')).toBe('old');
  });

  it('refuses to overwrite a release that is already published', () => {
    const publish = tempDir();
    publishRelease(fakeBuild('20261023T000000-aaaaaa', 'first'), publish);

    expect(() => publishRelease(fakeBuild('20261023T000000-aaaaaa', 'second'), publish)).toThrow(/already published/);
    expect(readFileSync(join(publish, '20261023T000000-aaaaaa', 'assets', 'main.js'), 'utf8')).toBe('first');
  });

  it('prunes all but the five newest releases besides the current one', () => {
    const publish = tempDir();
    const ids = Array.from({ length: 8 }, (_, i) => `20261023T00000${i}-aaaaaa`);
    for (const id of ids) publishRelease(fakeBuild(id), publish);

    expect(readdirSync(publish).filter(n => n !== POINTER_FILE).sort()).toEqual(ids.slice(2));
  });

  it('generates build ids usable as one URL path segment, sorted by time', () => {
    const a = newBuildId(new Date('2026-10-23T08:00:00Z'));
    const b = newBuildId(new Date('2026-10-23T09:00:00Z'));
    expect(a).toMatch(/^20261023T080000-[0-9a-f]{6}$/);
    expect([b, a].sort()).toEqual([a, b]);
  });
});
