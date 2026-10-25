// @ts-check
/**
 * Publishing interface releases, from a Vite build.
 *
 * Every production build gets a BUILD ID and a `ui-manifest.json` (path,
 * size and sha256 of every file).  With a publish directory, the build is
 * then PUBLISHED: copied into `<dir>/<build-id>/`, a frozen copy that is
 * never written again, and `<dir>/current.json` is switched to it.  The
 * release server (`kal-iroh-shell-releases`) serves that directory.
 *
 * - A device never sees a half-written release: the copy is completed
 *   under a temporary name and renamed, and only then is the pointer
 *   replaced, itself by a rename.
 * - A page that is already open keeps loading ITS OWN release's lazy
 *   chunks, because each release lives under its own directory and the
 *   newest few are kept.
 *
 * Loaded by Node from the consumer's `vite.config.ts`, hence plain
 * JavaScript (types in `vite.d.mts`).
 */
import { createHash, randomBytes } from 'node:crypto';
import {
  cpSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync,
  rmSync, statSync, writeFileSync,
} from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';

/** File, inside a build and a release, describing that build. */
export const MANIFEST_FILE = 'ui-manifest.json';
/** File, inside the publish directory, naming the current release. */
export const POINTER_FILE = 'current.json';
/** Releases kept besides the current one, for pages still running them. */
export const KEPT_RELEASES = 5;

/** Build ids are also directory and URL path segments. */
const BUILD_ID_RE = /^[0-9A-Za-z-]+$/;

/** A fresh build id: sortable UTC timestamp plus random suffix. */
export function newBuildId(now = new Date()) {
  const stamp = now.toISOString().replace(/[-:]/g, '').replace(/\..*$/, '');
  return `${stamp}-${randomBytes(3).toString('hex')}`;
}

/** @param {string} dir @returns {string[]} */
function walk(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
    const full = join(dir, entry.name);
    return entry.isDirectory() ? walk(full) : [full];
  });
}

/**
 * Manifest of every file under `dist`, the manifest itself excluded.
 * @param {string} dist @param {string} buildId
 */
export function buildManifest(dist, buildId) {
  if (!BUILD_ID_RE.test(buildId)) throw new Error(`invalid build id: ${buildId}`);
  const files = walk(dist)
    .map(full => relative(dist, full).split(sep).join('/'))
    .filter(path => path !== MANIFEST_FILE)
    .sort()
    .map(path => {
      const bytes = readFileSync(join(dist, path));
      return { path, size: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') };
    });
  return { build_id: buildId, files };
}

/** Write `ui-manifest.json` into `dist`. @param {string} dist @param {string} buildId */
export function writeManifest(dist, buildId) {
  const manifest = buildManifest(dist, buildId);
  writeFileSync(join(dist, MANIFEST_FILE), JSON.stringify(manifest, null, 2) + '\n');
  return manifest;
}

/** Replace `path` by a rename. @param {string} path @param {string} contents */
function writeAtomically(path, contents) {
  const tmp = `${path}.tmp-${process.pid}`;
  writeFileSync(tmp, contents);
  renameSync(tmp, path);
}

/** The release `current.json` names, or null. @param {string} publishDir */
export function readPointer(publishDir) {
  const path = join(publishDir, POINTER_FILE);
  if (!existsSync(path)) return null;
  return JSON.parse(readFileSync(path, 'utf8'));
}

/**
 * Publish the build in `dist` (which must hold its manifest) and make it
 * current.  Returns the build id.  Fails, publishing nothing, when the
 * manifest is missing or the release already exists.
 * @param {string} dist @param {string} publishDir
 */
export function publishRelease(dist, publishDir, now = new Date()) {
  const manifest = JSON.parse(readFileSync(join(dist, MANIFEST_FILE), 'utf8'));
  const id = manifest.build_id;
  if (!BUILD_ID_RE.test(id)) throw new Error(`invalid build id in manifest: ${id}`);
  mkdirSync(publishDir, { recursive: true });
  const target = join(publishDir, id);
  if (existsSync(target)) throw new Error(`release ${id} is already published`);
  const partial = join(publishDir, `.partial-${id}`);
  rmSync(partial, { recursive: true, force: true });
  cpSync(dist, partial, { recursive: true });
  renameSync(partial, target);
  writeAtomically(
    join(publishDir, POINTER_FILE),
    JSON.stringify({ build_id: id, published_at: now.toISOString() }) + '\n',
  );
  pruneReleases(publishDir, KEPT_RELEASES);
  return id;
}

/**
 * Delete all but the `keep` newest releases besides the current one.
 * @param {string} publishDir @param {number} keep
 */
export function pruneReleases(publishDir, keep) {
  const current = readPointer(publishDir)?.build_id;
  const releases = readdirSync(publishDir)
    .filter(name => BUILD_ID_RE.test(name) && name !== current)
    .filter(name => statSync(join(publishDir, name)).isDirectory())
    .sort()
    .reverse();
  const removed = releases.slice(keep);
  for (const name of removed) rmSync(join(publishDir, name), { recursive: true, force: true });
  return removed;
}

/**
 * Vite plugin: relative asset URLs (`base: './'`, so a release works
 * under any prefix), a manifest in every build, and a publish when
 * `publishDir` is given (`~/` is expanded).
 * @param {{ publishDir?: string }} [options]
 * @returns {import('vite').Plugin}
 */
export function kalIrohShellRelease(options = {}) {
  const buildId = newBuildId();
  /** @type {string} */
  let outDir = 'dist';
  return {
    name: 'kal-iroh-shell-release',
    apply: 'build',
    config() {
      return { base: './' };
    },
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    closeBundle() {
      writeManifest(outDir, buildId);
      const dir = options.publishDir?.replace(/^~(?=\/)/, process.env.HOME ?? '~');
      if (dir) {
        publishRelease(outDir, dir);
        console.log(`kal-iroh-shell: published release ${buildId} to ${dir}`);
      }
    },
  };
}
