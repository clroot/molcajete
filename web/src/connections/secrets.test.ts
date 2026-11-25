import {describe, expect, test} from 'bun:test';
import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {PROTOCOLS} from './protocols';
import {isSensitive, SENSITIVE_PARAMETERS} from './secrets';

/** The names in the gateway's `SENSITIVE_PARAMETERS`. */
function gatewayList(): string[] {
  const path = fileURLToPath(new URL('../../../crates/gateway/src/secrets.rs', import.meta.url));
  const source = readFileSync(path, 'utf8');
  const body = source.match(/pub const SENSITIVE_PARAMETERS: &\[&str\] = &\[([\s\S]*?)\];/)?.[1] ?? '';
  return [...body.matchAll(/"([^"]+)"/g)].map(m => m[1]);
}

describe('sensitive parameters', () => {
  test('match the gateway', () => {
    expect([...SENSITIVE_PARAMETERS].sort()).toEqual(gatewayList().sort());
  });

  test('include every credential field of the form', () => {
    for (const protocol of PROTOCOLS) {
      for (const field of protocol.fields) {
        if (field.kind === 'password') expect(isSensitive(field.name)).toBe(true);
      }
    }
    expect(isSensitive('private-key')).toBe(true);
    expect(isSensitive('hostname')).toBe(false);
  });
});
