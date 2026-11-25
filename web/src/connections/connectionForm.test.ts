import {describe, expect, test} from 'bun:test';
import {
  changeProtocol,
  clearSecret,
  keepSecret,
  otherStoredSecrets,
  secretState,
  emptyConnectionForm,
  extraParameter,
  slugify,
  toConnection,
  toConnectionForm,
} from './connectionForm';
import {requiredLabel} from './jumpHosts';

const rdp = {
  id: 'office',
  name: 'Office',
  protocol: 'rdp',
  guacdHost: null,
  guacdPort: null,
  guacdHandshakeTimeout: null,
  parameters: {hostname: '10.0.0.20', security: 'nla', 'color-depth': '16'},
  attributes: {},
};

describe('connection form', () => {
  test('splits protocol fields from other parameters', () => {
    const form = toConnectionForm(rdp);
    expect(form.fields).toEqual({hostname: '10.0.0.20', security: 'nla'});
    expect(form.extras.map(e => [e.name, e.value])).toEqual([['color-depth', '16']]);
  });

  test('keeps every value when the protocol changes and back', () => {
    const ssh = changeProtocol(toConnectionForm(rdp), 'ssh');
    expect(ssh.fields).toEqual({hostname: '10.0.0.20'});
    expect(ssh.extras.map(e => e.name).sort()).toEqual(['color-depth', 'security']);
    const back = changeProtocol(ssh, 'rdp');
    expect(back.fields).toEqual({hostname: '10.0.0.20', security: 'nla'});
  });

  test('round-trips to the API shape and drops empty values', () => {
    const form = toConnectionForm(rdp);
    form.extras.push(extraParameter('', 'ignored'), extraParameter('enable-audio', ''));
    form.fields.username = '';
    const result = toConnection(form);
    expect(result).toEqual({ok: true, connection: rdp});
  });

  test('new RDP connections resize the desktop to the window', () => {
    const form = changeProtocol({...emptyConnectionForm(), name: 'New'}, 'rdp', true);
    expect(form.fields).toEqual({'resize-method': 'display-update'});
    // Existing connections keep what they have.
    expect(changeProtocol(toConnectionForm({...rdp, protocol: 'vnc'}), 'rdp').fields).toEqual({
      hostname: '10.0.0.20',
      security: 'nla',
    });
    // A switch turned off is not stored, so guacd's default (none) applies.
    form.fields['resize-method'] = '';
    expect(toConnection(form)).toMatchObject({ok: true, connection: {parameters: {}}});
  });

  test('round-trips the display fields', () => {
    const display = {...rdp, parameters: {...rdp.parameters, 'resize-method': 'display-update', dpi: '144'}};
    const form = toConnectionForm(display);
    expect(form.fields).toMatchObject({'resize-method': 'display-update', dpi: '144'});
    expect(form.extras.map(e => e.name)).toEqual(['color-depth']);
    expect(toConnection(form)).toEqual({ok: true, connection: display});
    // Automatic is no `dpi` at all.
    form.fields.dpi = '';
    expect(toConnection(form)).toMatchObject({ok: true, connection: {parameters: {hostname: '10.0.0.20'}}});
    const result = toConnection(form);
    expect(result.ok && 'dpi' in result.connection.parameters).toBe(false);
  });

  test('uses the guacd port only with a host', () => {
    const form = {...toConnectionForm(rdp), guacdPort: '4823'};
    expect(toConnection(form)).toMatchObject({ok: true, connection: {guacdHost: null, guacdPort: null}});
    expect(toConnection({...form, guacdHost: ' 127.0.0.1 '})).toMatchObject({
      ok: true,
      connection: {guacdHost: '127.0.0.1', guacdPort: 4823},
    });
    expect(toConnection({...form, guacdPort: 'x'})).toMatchObject({ok: false});
    expect(toConnection({...form, name: ' '})).toMatchObject({ok: false});
  });

  test('round-trips the handshake timeout, with or without a guacd host', () => {
    const slow = {...rdp, guacdHandshakeTimeout: 60};
    const form = toConnectionForm(slow);
    expect(form.guacdHandshakeTimeout).toBe('60');
    expect(toConnection(form)).toEqual({ok: true, connection: slow});
    expect(toConnection({...form, guacdHandshakeTimeout: ' '})).toMatchObject({
      ok: true,
      connection: {guacdHandshakeTimeout: null},
    });
    for (const bad of ['0', '3601', '1.5', 'x']) {
      expect(toConnection({...form, guacdHandshakeTimeout: bad})).toMatchObject({ok: false});
    }
  });

  test('keeps recording choices as attributes, not parameters', () => {
    const recorded = {...rdp, attributes: {'record-sessions': 'true', 'record-exclude-mouse': 'true'}};
    const form = toConnectionForm(recorded);
    expect(form.recording).toEqual({record: true, keys: false, mouse: false, molcajeteFormats: false});
    expect(form.extras.map(e => e.name)).toEqual(['color-depth']);
    expect(toConnection(form)).toEqual({ok: true, connection: recorded});
    const keys = toConnection({...form, recording: {...form.recording, keys: true, mouse: true}});
    expect(keys).toMatchObject({
      ok: true,
      connection: {
        attributes: {'record-sessions': 'true', 'record-keys': 'true'},
        parameters: rdp.parameters,
      },
    });
    expect(emptyConnectionForm().recording).toEqual({
      record: false,
      keys: false,
      mouse: true,
      molcajeteFormats: false,
    });
  });

  test('stored credentials are kept unless replaced or cleared', () => {
    const stored = {...rdp, storedSecrets: ['password', 'sftp-password']};
    const form = toConnectionForm(stored);
    expect(secretState(form, 'password')).toBe('stored');
    expect(secretState(form, 'username')).toBe('none');
    expect(otherStoredSecrets(form)).toEqual(['sftp-password']);
    // Saved untouched: nothing about them is sent, so the gateway keeps them.
    expect(toConnection(form)).toEqual({ok: true, connection: rdp});

    // A typed value replaces one; clearing removes one.
    const replaced = {...form, fields: {...form.fields, password: 'new'}};
    expect(toConnection(replaced)).toMatchObject({ok: true, connection: {parameters: {password: 'new'}}});
    const cleared = clearSecret(replaced, 'sftp-password');
    expect(secretState(cleared, 'sftp-password')).toBe('cleared');
    expect(toConnection(cleared)).toMatchObject({ok: true, connection: {clearSecrets: ['sftp-password']}});
    expect(toConnection(keepSecret(cleared, 'sftp-password'))).not.toMatchObject({
      connection: {clearSecrets: expect.anything()},
    });

    // Clearing drops what was typed; typing again after clearing replaces.
    const clearedPassword = clearSecret(replaced, 'password');
    expect(clearedPassword.fields.password).toBeUndefined();
    expect(toConnection(clearedPassword)).toMatchObject({ok: true, connection: {clearSecrets: ['password']}});
    const retyped = {...clearedPassword, fields: {...clearedPassword.fields, password: 'again'}};
    const result = toConnection(retyped);
    expect(result).toMatchObject({ok: true, connection: {parameters: {password: 'again'}}});
    expect(result.ok && result.connection.clearSecrets).toBeFalsy();
  });

  test('suggests URL-safe ids', () => {
    expect(slugify('  My Desktop (VNC) ')).toBe('my-desktop-vnc');
  });

  test('a new connection goes into the chosen folder', () => {
    const form = {...emptyConnectionForm('databases'), name: 'Postgres', id: 'pg'};
    expect(toConnection(form)).toMatchObject({ok: true, connection: {parentId: 'databases'}});
    expect(toConnection({...form, parentId: ''})).not.toMatchObject({
      connection: {parentId: expect.anything()},
    });
  });
});

describe('jump hosts', () => {
  const ssh = {
    ...rdp,
    protocol: 'ssh',
    parameters: {
      hostname: 'db',
      'jump-hosts': 'admin@bastion',
      'jump-host-password': 'pw',
      'jump-host-2-password': 'other',
    },
  };

  test('shared jump host values are fields for every protocol; per-hop ones are extras', () => {
    const form = toConnectionForm(ssh);
    expect(form.fields).toEqual({hostname: 'db', 'jump-hosts': 'admin@bastion', 'jump-host-password': 'pw'});
    expect(form.extras.map(e => e.name)).toEqual(['jump-host-2-password']);
    const vnc = changeProtocol(form, 'vnc');
    expect(vnc.fields['jump-hosts']).toBe('admin@bastion');
    expect(toConnection(vnc)).toMatchObject({ok: true, connection: {parameters: ssh.parameters}});
  });

  test('prompts get readable labels', () => {
    expect(requiredLabel('jump-host-password')).toBe('Jump host password');
    expect(requiredLabel('jump-host-2-password')).toBe('Jump host 2 password');
    expect(requiredLabel('password')).toBe('password');
  });
});
