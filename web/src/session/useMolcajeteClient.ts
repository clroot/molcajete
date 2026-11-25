import {useCallback, useEffect, useRef, useState, type RefObject} from 'react';
import {loadClient, loadReporter, tunnelBase, type MolcajeteSession, type SessionEvent} from './client';

type Reporter = InstanceType<Awaited<ReturnType<typeof loadReporter>>['SessionReporter']>;

// `?main` runs the client on the main thread instead of a worker.
const forceMainThread = new URLSearchParams(location.search).has('main');
// `?text` keeps the tunnel on the Guacamole text protocol (base64 data).
const forceTextTunnel = new URLSearchParams(location.search).has('text');

type Options = {
  displayRef: RefObject<HTMLDivElement | null>;
  connectionId: string;
  onEvent: (event: SessionEvent) => void;
  /** The client could not be created (no WebGL2, wasm failed to load…). */
  onStartFailed: (message: string) => void;
};

/**
 * Creates the wasm client in `displayRef`, connects it to `connectionId`
 * and frees it on unmount. `connect()` reconnects the same client.
 */
export function useMolcajeteClient({displayRef, connectionId, onEvent, onStartFailed}: Options) {
  const clientRef = useRef<MolcajeteSession | null>(null);
  const reporterRef = useRef<Reporter | null>(null);
  const [mode, setMode] = useState<MolcajeteSession['mode'] | null>(null);

  const connect = useCallback(async () => {
    const client = clientRef.current;
    if (!client) return;
    reporterRef.current?.stop();
    reporterRef.current = null;
    // Performance reports are optional; the session works without them.
    const reporting = await loadReporter().catch(() => null);
    if (reporting && (await reporting.clientReportsEnabled())) {
      reporterRef.current = new reporting.SessionReporter(connectionId, client.mode);
    }
    let url = client.tunnel_url(tunnelBase(), connectionId);
    if (reporterRef.current) url += `&session=${encodeURIComponent(reporterRef.current.session)}`;
    client.connect(url);
  }, [connectionId]);

  useEffect(() => {
    let isCurrent = true;
    (async () => {
      try {
        const {MolcajeteSession} = await loadClient();
        const client = await MolcajeteSession.create(displayRef.current!, {
          worker: forceMainThread ? false : undefined,
          binary: forceTextTunnel ? false : undefined,
        });
        if (!isCurrent) {
          client.free();
          return;
        }
        clientRef.current = client;
        setMode(client.mode);
        client.set_event_handler((type, detail) => {
          const event = {type, detail} as SessionEvent;
          if (event.type === 'stats') reporterRef.current?.add(event.detail);
          onEvent(event);
        });
        client.set_scale_mode('fit');
        await connect();
      } catch (err) {
        if (isCurrent) onStartFailed(err instanceof Error ? err.message : String(err));
      }
    })();
    const flushReports = () => reporterRef.current?.flush({beacon: true});
    window.addEventListener('pagehide', flushReports);
    return () => {
      isCurrent = false;
      window.removeEventListener('pagehide', flushReports);
      reporterRef.current?.stop();
      reporterRef.current = null;
      clientRef.current?.free();
      clientRef.current = null;
    };
  }, [displayRef, connect, onEvent, onStartFailed]);

  /** `client()` is for event handlers; render with `mode` instead. */
  return {client: () => clientRef.current, mode, connect};
}
