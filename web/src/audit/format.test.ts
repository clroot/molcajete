import {describe, expect, test} from 'bun:test';
import {auditQuery} from '../api';
import {actionGroups, changeRows, localDayStart} from './format';

describe('audit log', () => {
  test('shows changes, lists and details as rows', () => {
    expect(
      changeRows({
        role: {from: 'user', to: 'admin'},
        password: {from: null, to: '[redacted]'},
        groups: {added: ['ops'], removed: ['staff', 'qa']},
        connections: {added: ['desk']},
        reason: 'wrong_password',
      }),
    ).toEqual([
      {field: 'role', before: 'user', after: 'admin'},
      {field: 'password', before: '—', after: '[redacted]'},
      {field: 'groups', before: 'Removed staff, qa', after: 'Added ops'},
      {field: 'connections', before: '—', after: 'Added desk'},
      {field: 'reason', before: '—', after: 'wrong_password'},
    ]);
    expect(changeRows(null)).toEqual([]);
  });

  test('groups actions by their first part', () => {
    expect(actionGroups(['login.success', 'logout', 'user.create', 'user.update'])).toEqual([
      {prefix: 'login', actions: ['login.success']},
      {prefix: 'logout', actions: ['logout']},
      {prefix: 'user', actions: ['user.create', 'user.update']},
    ]);
  });

  test('builds the query without empty filters', () => {
    expect(auditQuery({actor: 'root', action: '', target: undefined}, {limit: 50, offset: 0})).toBe(
      'actor=root&limit=50&offset=0',
    );
  });

  test('turns dates into local day boundaries', () => {
    expect(localDayStart('2026-11-25')).toBe(new Date(2026, 10, 25).getTime());
    expect(localDayStart('2026-12-31', 1)).toBe(new Date(2027, 0, 1).getTime());
  });
});
