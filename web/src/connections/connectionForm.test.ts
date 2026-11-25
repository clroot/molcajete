import {describe, expect, test} from 'bun:test';
import {changeProtocol, extraParameter, slugify, toConnection, toConnectionForm} from './connectionForm';

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
