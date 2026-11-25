import {describe, expect, test} from 'bun:test';
import {
  changeProtocol,
  emptyConnectionForm,
  extraParameter,
  slugify,
  toConnection,
  toConnectionForm,
} from './connectionForm';

const rdp = {
  id: 'office',
  name: 'Office',
  protocol: 'rdp',
  guacdHost: null,
  guacdPort: null,
  parameters: {hostname: '10.0.0.20', security: 'nla', 'color-depth': '16'},
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

  test('suggests URL-safe ids', () => {
    expect(slugify('  My Desktop (VNC) ')).toBe('my-desktop-vnc');
  });
});
