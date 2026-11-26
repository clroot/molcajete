// Pieces of IronRDP sessions that do not need the browser: the tunnel URL
// and what a failure says.

/** The tunnel IronRDP's client opens: binary RDCleanPath and RDP, no subprotocol. */
export function ironRdpTunnelUrl(page: Pick<Location, 'href'>, connectionId: string, token: string): string {
  const url = new URL('websocket-tunnel', page.href);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.search = new URLSearchParams({connection: connectionId, ironrdp: token}).toString();
  url.hash = '';
  return url.toString();
}

/** IronRDP's error kinds (`IronErrorKind` of @devolutions/iron-remote-desktop). */
const KIND = {
  wrongPassword: 1,
  logonFailure: 2,
  accessDenied: 3,
  rdCleanPath: 4,
  proxyConnect: 5,
  negotiationFailure: 6,
} as const;

type IronErrorLike = {kind: () => number; backtrace: () => string};

const isIronError = (err: unknown): err is IronErrorLike =>
  typeof err === 'object' &&
  err !== null &&
  typeof (err as IronErrorLike).kind === 'function' &&
  typeof (err as IronErrorLike).backtrace === 'function';

/** A title and a sentence for a session that could not start or ended in an error. */
export function describeIronRdpError(err: unknown): {title: string; detail: string} {
  if (!isIronError(err)) {
    return {title: 'Connection error', detail: err instanceof Error ? err.message : String(err)};
  }
  // The first line of the backtrace is IronRDP's own message.
  const message = err.backtrace().split('\n')[0]?.trim() ?? '';
  switch (err.kind()) {
    case KIND.wrongPassword:
    case KIND.logonFailure:
    case KIND.accessDenied:
      return {
        title: 'The server refused the sign-in',
        detail: 'Check the user name and password stored for this connection.',
      };
    case KIND.rdCleanPath:
      return {
        title: 'The remote computer could not be reached',
        detail: 'molcajeted could not connect to it, or the server does not accept this kind of session.',
      };
    case KIND.proxyConnect:
      return {title: 'The gateway refused the session', detail: message};
    case KIND.negotiationFailure:
      return {title: 'The server refused the connection', detail: message};
    default:
      return {title: 'Connection error', detail: message};
  }
}

/** The remote desktop IronRDP's client asks for: a size in device pixels and a scale. */
export type RemoteDesktop = {width: number; height: number; scalePercent: number};

/**
 * The desktop for a window of `css` CSS pixels on a screen with
 * `pixelRatio` device pixels per CSS pixel. Windows draws it at full
 * resolution and scales text and apps up by `scalePercent`, so the picture
 * is not stretched by the browser. `dpi` is the chosen remote scale (96 per
 * 100 %), `'auto'` for the screen's own.
 */
export function remoteDesktop(
  css: {width: number; height: number},
  pixelRatio: number,
  dpi: number | 'auto',
): RemoteDesktop {
  const ratio = pixelRatio > 0 ? pixelRatio : 1;
  // Display Control takes 200 to 8192 pixels each way, the width even.
  const width = Math.min(8192, Math.max(200, Math.round(css.width * ratio)));
  const height = Math.min(8192, Math.max(200, Math.round(css.height * ratio)));
  const percent = dpi === 'auto' ? ratio * 100 : (dpi * 100) / 96;
  return {
    width: width - (width % 2),
    height,
    scalePercent: Math.min(500, Math.max(100, Math.round(percent))),
  };
}
