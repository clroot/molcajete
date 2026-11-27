# @devolutions/iron-remote-desktop-rdp 0.8.0, with one fix

A build of IronRDP's RDP package for the web, used by the IronRDP sessions
of `web/src/session/IronRdpSessionPage.tsx` instead of the published 0.8.0.

The published 0.8.0 drops the bytes its connector has read past the end of
the connection sequence. A Windows 11 host often sends its DRDYNVC
Capabilities Request in the same read as the Font Map PDU; with the request
lost and the graphics pipeline offered, the host opens no dynamic channel
and the screen stays black. The fix is Devolutions/IronRDP#2546
(issue Devolutions/IronRDP#2545).

## Source

- Repository: https://github.com/clroot/IronRDP, branch
  `molcajete/iron-remote-desktop-rdp-0.8.0-keep-leftover`
- Commit: `90e94bdea2a521b522c12f9f6206397ab1563ce8`, the commit of
  Devolutions/IronRDP#2546 on the tag `npm-iron-remote-desktop-rdp-v0.8.0`
- `iron-remote-desktop-rdp.js` SHA-256:
  `120f1045e0adf479d3d1defae80a4c85e1d118c8c79269d6c163d4c1ff8e4185`

## Rebuilding

With the Rust toolchain of that repository (`rust-toolchain.toml`), the
`wasm32-unknown-unknown` target and `wasm-pack`:

```sh
git clone --branch molcajete/iron-remote-desktop-rdp-0.8.0-keep-leftover \
  https://github.com/clroot/IronRDP.git
cd IronRDP/web-client/iron-remote-desktop-rdp
npm install
npm run build
cp dist/package.json dist/index.d.ts dist/iron-remote-desktop-rdp.js \
  <molcajete>/web/vendor/iron-remote-desktop-rdp/
```

## Removing

Once a published release contains Devolutions/IronRDP#2546, depend on it
again in `web/package.json` and delete this directory (GH-156).
