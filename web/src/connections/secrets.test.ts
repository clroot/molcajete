import {describe, expect, test} from 'bun:test';
import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {PROTOCOLS} from './protocols';
import {JUMP_HOST_FIELDS} from './jumpHosts';
import {isSensitive, SECRET_NAMES, SECRET_PARTS} from './secrets';

/** The strings of the gateway's `const <name>: &[&str] = &[…];` in sensitive.rs. */
function gatewayList(name: string): string[] {
  const path = fileURLToPath(new URL('../../../crates/gateway/src/sensitive.rs', import.meta.url));
  const source = readFileSync(path, 'utf8');
  const body = source.match(new RegExp(`const ${name}: &\\[&str\\] = &\\[([\\s\\S]*?)\\];`))?.[1] ?? '';
  return [...body.matchAll(/"([^"]+)"/g)].map(m => m[1]);
}

describe('sensitive parameters', () => {
  test('match the gateway', () => {
    expect([...SECRET_NAMES].sort()).toEqual(gatewayList('SECRET_NAMES').sort());
    expect([...SECRET_PARTS].sort()).toEqual(gatewayList('SECRET_PARTS').sort());
  });

  test('include every credential field of the form', () => {
    const fields = [...PROTOCOLS.flatMap(p => p.fields), ...JUMP_HOST_FIELDS];
    for (const field of fields) {
      if (field.kind === 'password') expect(isSensitive(field.name)).toBe(true);
    }
    for (const name of ['private-key', 'jump-host-private-key', 'jump-host-2-password', 'sftp-passphrase']) {
      expect(isSensitive(name)).toBe(true);
    }
    for (const name of ['hostname', 'username', 'jump-host-host-key', 'password-regex']) {
      expect(isSensitive(name)).toBe(false);
    }
  });
});
