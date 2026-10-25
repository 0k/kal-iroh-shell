// @vitest-environment node
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';
import * as protocol from '../src/protocol';
import * as release from '../src/vite.mjs';

/** `pub const NAME: &str = "value";` of the Rust protocol crate. */
function rustConstants(): Record<string, string> {
  const src = readFileSync(
    fileURLToPath(new URL('../../crates/kal-iroh-shell-protocol/src/lib.rs', import.meta.url)),
    'utf8',
  );
  return Object.fromEntries(
    [...src.matchAll(/pub const (\w+): &str = "([^"]*)";/g)].map(([, name, value]) => [name, value]),
  );
}

describe('wire names', () => {
  it('match the Rust protocol crate', () => {
    const rust = rustConstants();
    for (const [name, value] of Object.entries(protocol)) {
      expect({ name, value }).toEqual({ name, value: rust[name] });
    }
    expect(release.MANIFEST_FILE).toBe(rust.MANIFEST_FILE);
    expect(release.POINTER_FILE).toBe(rust.POINTER_FILE);
  });
});
