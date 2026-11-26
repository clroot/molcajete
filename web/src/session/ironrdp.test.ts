import {describe, expect, it} from 'bun:test';
import {describeIronRdpError, ironRdpTunnelUrl} from './ironrdp';

describe('ironRdpTunnelUrl', () => {
  it('opens the tunnel next to the page with the token', () => {
    expect(ironRdpTunnelUrl({href: 'https://gw.example/molcajete/#/connect/desk'}, 'desk pc', 'abc')).toBe(
      'wss://gw.example/molcajete/websocket-tunnel?connection=desk+pc&ironrdp=abc',
    );
    expect(ironRdpTunnelUrl({href: 'http://127.0.0.1:8080/'}, 'desk', 't')).toBe(
      'ws://127.0.0.1:8080/websocket-tunnel?connection=desk&ironrdp=t',
    );
  });
});

describe('describeIronRdpError', () => {
  const ironError = (kind: number, backtrace: string) => ({kind: () => kind, backtrace: () => backtrace});

  it('explains a refused sign-in without IronRDP internals', () => {
    expect(describeIronRdpError(ironError(2, 'logon failure\n at ...')).title).toBe(
      'The server refused the sign-in',
    );
  });

  it('keeps the first line of other errors', () => {
    expect(describeIronRdpError(ironError(0, 'websocket closed\nstack'))).toEqual({
      title: 'Connection error',
      detail: 'websocket closed',
    });
  });

  it('handles plain errors', () => {
    expect(describeIronRdpError(new Error('no wasm'))).toEqual({
      title: 'Connection error',
      detail: 'no wasm',
    });
  });
});
