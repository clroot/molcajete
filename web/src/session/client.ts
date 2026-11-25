// Types for the page-side wasm client glue in public/molcajete.js. It is
// loaded at runtime (not bundled) so it keeps resolving `./pkg/` and
// `./molcajete-worker.js` next to itself.

export type SessionStats = {
  displayWidth: number;
  displayHeight: number;
  framesPerSecond: number;
  compositesPerSecond: number;
  drawCallsPerSecond: number;
  instructionsPerSecond: number;
  bytesPerSecond: number;
  frameLagMs: number;
  frameLagMsAvg?: number;
  frameLagMsMax?: number;
  frameReceiveMsAvg?: number;
  frameReceiveMsMax?: number;
  imagesPerSecond?: number;
  imageDecodeMsAvg?: number;
  imageDecodeMsMax?: number;
  /** Null for a second in which no frame was drawn to the canvas. */
  framePresentMsAvg?: number | null;
  framePresentMsMax?: number;
  rttMs: number | null;
  queuedOps: number;
};

export type SessionEvent =
  | {
      type: 'state';
      detail: 'idle' | 'connecting' | 'waiting' | 'connected' | 'disconnecting' | 'disconnected';
    }
  | {type: 'error'; detail: {message: string; name: string; code?: number}}
  | {type: 'name'; detail: string}
  | {type: 'clipboard'; detail: string}
  | {type: 'required'; detail: string[]}
  | {type: 'unstable'; detail: boolean}
  /** Whether the server can change the remote display during the session (molcajeted only). */
  | {type: 'display-control'; detail: boolean}
  | {type: 'stats'; detail: SessionStats}
  /** Target of the link under the pointer in a terminal, or null (molcajeted SSH only). */
  | {type: 'link'; detail: string | null}
  /** The terminal has no selection any more (molcajeted SSH only); a selection arrives as `clipboard`. */
  | {type: 'selection'; detail: boolean}
  /** The copy shortcut wrote the selection to the local clipboard, or could not. */
  | {type: 'copy'; detail: {ok: boolean}};

export interface MolcajeteSession {
  readonly mode: 'worker' | 'main';
  set_event_handler(handler: (type: string, detail: unknown) => void): void;
  tunnel_url(base: string, connection: string): string;
  connect(url: string): void;
  disconnect(): void;
  focus(): void;
  send_clipboard(text: string): void;
  /** Copy, paste and link shortcuts of terminals; other protocols get those keys as they are. */
  set_terminal_shortcuts(enabled: boolean): void;
  send_argv(name: string, value: string): void;
  send_ctrl_alt_del(): void;
  set_scale_mode(mode: 'fit' | 'native'): void;
  /** Whether window size changes are sent to the server; turning it on sends the current size. */
  set_auto_resize(enabled: boolean): void;
  /** The remote host's IME composes text; keys are sent as pressed. */
  set_remote_composition(enabled: boolean): void;
  release_keys(): void;
  free(): void;
}

type ClientModule = {
  MolcajeteSession: {
    create(
      container: HTMLElement,
      options?: {worker?: boolean; binary?: boolean; zstd?: boolean},
    ): Promise<MolcajeteSession>;
  };
};

type ReporterModule = {
  clientReportsEnabled(): Promise<boolean>;
  SessionReporter: new (
    connection: string,
    mode: string,
  ) => {
    session: string;
    add(stats: SessionStats): void;
    stop(): void;
    flush(options?: {beacon?: boolean}): void;
  };
};

const load = <T>(file: string) =>
  import(/* @vite-ignore */ new URL(file, document.baseURI).href) as Promise<T>;

export const loadClient = () => load<ClientModule>('molcajete.js');
export const loadReporter = () => load<ReporterModule>('client-report.js');

export function tunnelBase(): string {
  const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const path = location.pathname.replace(/[^/]*$/, '');
  return `${scheme}//${location.host}${path}websocket-tunnel`;
}

export function formatStats(s: SessionStats, mode: string): string {
  const kb = (s.bytesPerSecond / 1024).toFixed(1);
  return [
    `renderer  WebGL2 (${mode} thread)`,
    `display   ${s.displayWidth}×${s.displayHeight}`,
    `frames    ${s.framesPerSecond.toFixed(1)}/s`,
    `paints    ${s.compositesPerSecond.toFixed(1)}/s`,
    `draws     ${s.drawCallsPerSecond.toFixed(0)}/s`,
    `instr     ${s.instructionsPerSecond.toFixed(0)}/s`,
    `recv      ${kb} KiB/s`,
    `lag       ${s.frameLagMs.toFixed(1)} ms`,
    `present   ${s.framePresentMsAvg == null ? '-' : s.framePresentMsAvg.toFixed(1) + ' ms'}`,
    `rtt       ${s.rttMs == null ? '-' : s.rttMs.toFixed(0) + ' ms'}`,
    `queued    ${s.queuedOps}`,
  ].join('\n');
}
