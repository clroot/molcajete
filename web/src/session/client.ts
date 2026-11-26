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

/** How a modifier is held from the page: for the next key, or until it is turned off. */
export type Hold = 'off' | 'once' | 'locked';

/** Ctrl and Alt as the terminal key bar holds them. */
export type HeldModifiers = {ctrl: Hold; alt: Hold};

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
  /** Ctrl and Alt held with `hold_modifier`, after every change. */
  | {type: 'modifiers'; detail: HeldModifiers}
  /** The copy shortcut wrote the selection to the local clipboard, or could not. */
  | {type: 'copy'; detail: {ok: boolean}}
  /** Whether an audio stream is open while the browser holds its sound back until a click or key press. */
  | {type: 'audio'; detail: {blocked: boolean}}
  /** A filesystem the server exposes for file transfer (SFTP, the RDP drive). */
  | {type: 'filesystem'; detail: Filesystem}
  | {type: 'filesystem-removed'; detail: {object: number}}
  /** Progress of an upload or a download. */
  | {type: 'transfer'; detail: Transfer};

export type Filesystem = {
  object: number;
  name: string;
  /** False when the connection does not allow it; stock guacd does not say, and may refuse later. */
  canDownload: boolean;
  canUpload: boolean;
};

export type FileEntry = {
  /** Path inside the filesystem, from `/`. */
  path: string;
  name: string;
  type: 'directory' | 'file';
  /** Bytes, when the server says (molcajeted). */
  size: number | null;
};

export type Transfer = {
  id: string;
  direction: 'upload' | 'download';
  name: string;
  path: string;
  state: 'active' | 'done' | 'failed' | 'cancelled';
  bytes: number;
  total: number | null;
  error?: string;
};

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
  /** Presses and releases a key (an X11 keysym) with the held modifiers. */
  press_key(keysym: number): void;
  /** Holds Ctrl or Alt for the next key (`once`) or until `off` (`locked`); answered by `modifiers` events. */
  hold_modifier(name: keyof HeldModifiers, hold: Hold): void;
  /** Pastes text into a terminal, as its paste shortcut does. */
  paste_text(text: string): void;
  set_scale_mode(mode: 'fit' | 'native'): void;
  /** Whether window size changes are sent to the server; turning it on sends the current size. */
  set_auto_resize(enabled: boolean): void;
  /** The remote host's IME composes text; keys are sent as pressed. */
  set_remote_composition(enabled: boolean): void;
  release_keys(): void;
  /** Lists a directory of a filesystem (from a `filesystem` event). */
  list(object: number, path: string): Promise<FileEntry[]>;
  /** Uploads into a filesystem at `path`, or without them into the server's upload directory. Returns the transfer id. */
  upload(file: File, target?: {object?: number; path?: string}): string;
  /** Downloads a file; call it from the click. Resolves to the transfer id, or null when the save dialog was cancelled. */
  download(
    object: number,
    path: string,
    options?: {name?: string; size?: number | null},
  ): Promise<string | null>;
  cancel_transfer(id: string): void;
  /** Controls the player of a client created with `playback: true` (public/molcajete-player.js). */
  playback(command: 'load', url: string): void;
  playback(command: 'seek', time: number): void;
  playback(command: 'speed', speed: number): void;
  playback(command: 'play' | 'pause' | 'close'): void;
  free(): void;
}

/** What the player of session recordings reports (`playback` events). */
export type PlaybackStatus =
  | {state: 'loading'; loaded: number; total: number}
  | {state: 'error'; message: string}
  | {
      state: 'ready' | 'playing' | 'paused' | 'seeking' | 'ended';
      /** Milliseconds from the start of the recording. */
      time: number;
      duration: number;
      /** Unix milliseconds of the start. */
      start: number;
      speed: number;
    };

type ClientModule = {
  MolcajeteSession: {
    /** `file`: written to disk as it arrives; `blob`: collected in memory first. */
    readonly downloadMethod: 'file' | 'blob';
    readonly BLOB_WARNING_BYTES: number;
    create(
      container: HTMLElement,
      options?: {
        worker?: boolean;
        binary?: boolean;
        zstd?: boolean;
        playback?: boolean;
        /** Where audio plays; made during a user gesture so that the browser lets it run. */
        audioContext?: AudioContext;
      },
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
