#!/usr/bin/env node
/**
 * Deliver this package to consumers as a tarball committed in their
 * `vendor/` directory.
 *
 * Usage:  npm run vendor -- <consumer-dir> [<consumer-dir>…] [--dry-run]
 *
 * <consumer-dir> holds the consumer's `package.json`.  Every consumer is
 * validated before any is written; a failure writes nothing.  For each
 * consumer it copies `vendor/0k-kal-iroh-shell-<version>.tgz`, removes
 * older tarballs of this package, and points the dependency at the new
 * one.  It does not run `npm install` and does not commit: both are the
 * operator's, through a diff someone reads.
 */
import { execFileSync } from 'node:child_process';
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, unlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const OWNER = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const dryRun = args.includes('--dry-run');
const consumers = args.filter(a => a !== '--dry-run').map(d => resolve(d));

function fail(message) {
  console.error(`vendor: ${message}`);
  console.error('vendor: NOTHING was written.');
  process.exit(1);
}

if (consumers.length === 0) fail('usage: npm run vendor -- <consumer-dir>… [--dry-run]');
const pkg = JSON.parse(readFileSync(join(OWNER, 'package.json'), 'utf8'));
const tarName = `${pkg.name.replace('@', '').replace('/', '-')}-${pkg.version}.tgz`;
const prefix = `${pkg.name.replace('@', '').replace('/', '-')}-`;

const plans = consumers.map(dir => {
  const manifest = join(dir, 'package.json');
  if (!existsSync(manifest)) fail(`${dir}: no package.json`);
  const json = JSON.parse(readFileSync(manifest, 'utf8'));
  const section = ['dependencies', 'devDependencies'].find(s => json[s]?.[pkg.name]);
  if (!section) fail(`${dir}: does not depend on ${pkg.name}; add it first`);
  return { dir, manifest, json, section };
});

const work = mkdtempSync(join(tmpdir(), 'kal-iroh-shell-vendor-'));
try {
  execFileSync('npm', ['pack', '--pack-destination', work], { cwd: OWNER, stdio: 'inherit' });
  const tarball = join(work, tarName);
  if (!existsSync(tarball)) fail(`npm pack did not produce ${tarName}`);
  for (const { dir, manifest, json, section } of plans) {
    const vendor = join(dir, 'vendor');
    const stale = existsSync(vendor) ? readdirSync(vendor).filter(f => f.startsWith(prefix) && f !== tarName) : [];
    console.error(`vendor: ${dir}: ${tarName}${stale.length ? `, removing ${stale.join(', ')}` : ''}`);
    if (dryRun) continue;
    mkdirSync(vendor, { recursive: true });
    copyFileSync(tarball, join(vendor, tarName));
    for (const f of stale) unlinkSync(join(vendor, f));
    json[section][pkg.name] = `file:vendor/${tarName}`;
    writeFileSync(manifest, JSON.stringify(json, null, 2) + '\n');
  }
} finally {
  rmSync(work, { recursive: true, force: true });
}
console.error(dryRun ? 'vendor: dry run, nothing written.' : 'vendor: done; run `npm install` in each consumer.');
