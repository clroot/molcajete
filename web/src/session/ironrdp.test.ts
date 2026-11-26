import {describe, expect, it} from 'bun:test';
import {describeIronRdpError, ironRdpTunnelUrl, remoteDesktop} from './ironrdp';

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

describe('remoteDesktop', () => {
  it('asks for device pixels and scales up by the screen', () => {
    // A 12.9-inch iPad: 1366×1024 CSS pixels, two device pixels each.
    expect(remoteDesktop({width: 1366, height: 1024}, 2, 'auto')).toEqual({
      width: 2732,
      height: 2048,
      scalePercent: 200,
    });
    expect(remoteDesktop({width: 1280, height: 800}, 1, 'auto')).toEqual({
      width: 1280,
      height: 800,
      scalePercent: 100,
    });
  });

  it('uses a chosen remote scale instead of the screen', () => {
    expect(remoteDesktop({width: 1366, height: 1024}, 2, 168).scalePercent).toBe(175);
    expect(remoteDesktop({width: 1366, height: 1024}, 2, 96).scalePercent).toBe(100);
  });

  it('stays within what RDP accepts', () => {
    expect(remoteDesktop({width: 5000, height: 50}, 2, 'auto')).toEqual({
      width: 8192,
      height: 200,
      scalePercent: 200,
    });
    // Odd widths are rounded down to even; fractional ratios are rounded.
    expect(remoteDesktop({width: 1001, height: 700}, 1.5, 'auto')).toEqual({
      width: 1502,
      height: 1050,
      scalePercent: 150,
    });
    expect(remoteDesktop({width: 800, height: 600}, 0, 2000).scalePercent).toBe(500);
  });
});
