// Full-screen remote session: the wasm client renders into `display`;
// connection states show over it, and Ctrl+Alt+Shift opens the session menu
// (the same shortcut as Apache Guacamole).
import {useCallback, useEffect, useRef, useState, type FormEvent} from 'react';
import * as stylex from '@stylexjs/stylex';
import {Card} from '@astryxdesign/core/Card';
import {Center} from '@astryxdesign/core/Center';
import {VStack, HStack} from '@astryxdesign/core/Stack';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Button} from '@astryxdesign/core/Button';
import {Spinner} from '@astryxdesign/core/Spinner';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {TextInput} from '@astryxdesign/core/TextInput';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Kbd} from '@astryxdesign/core/Kbd';
import {api} from '../api';
import {href, navigate} from '../route';
import {
  formatStats,
  loadClient,
  loadReporter,
  tunnelBase,
  type MolcajeteSession,
  type SessionEvent,
  type SessionStats,
} from './client';

const styles = stylex.create({
  display: {
    position: 'fixed',
    inset: 0,
    backgroundColor: 'black',
    overflow: 'hidden',
  },
  overlay: {
    position: 'fixed',
    inset: 0,
    backgroundColor: 'rgb(0 0 0 / 0.55)',
  },
  overlayCard: {
    width: '100%',
    maxWidth: 420,
  },
  stats: {
    position: 'fixed',
    top: 8,
    left: 8,
    margin: 0,
    pointerEvents: 'none',
    whiteSpace: 'pre',
  },
  unstable: {
    position: 'fixed',
    top: 8,
    left: '50%',
    transform: 'translateX(-50%)',
    pointerEvents: 'none',
  },
});

type Overlay =
  | {kind: 'connecting'; detail?: string}
  | {kind: 'error'; title: string; detail: string}
  | {kind: 'disconnected'}
  | null;

type ScaleMode = 'fit' | 'native';

// `?main` runs the client on the main thread instead of a worker.
const forceMainThread = new URLSearchParams(location.search).has('main');

function isSecretParameter(name: string) {
  return /password|passphrase/.test(name);
}

export default function SessionPage({connectionId}: {connectionId: string}) {
  const displayRef = useRef<HTMLDivElement>(null);
  const clientRef = useRef<MolcajeteSession | null>(null);
  const reporterRef = useRef<InstanceType<Awaited<ReturnType<typeof loadReporter>>['SessionReporter']> | null>(
    null,
  );

  const [overlay, setOverlay] = useState<Overlay>({kind: 'connecting'});
  const [name, setName] = useState(connectionId);
  const [required, setRequired] = useState<string[] | null>(null);
  const [credentials, setCredentials] = useState<Record<string, string>>({});
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const [clipboard, setClipboard] = useState('');
  const [scaleMode, setScaleMode] = useState<ScaleMode>('fit');
  const [showStats, setShowStats] = useState(false);
  const [stats, setStats] = useState<SessionStats | null>(null);
  const [unstable, setUnstable] = useState(false);

  const showStatsRef = useRef(showStats);
  showStatsRef.current = showStats;

  const handleEvent = useCallback((event: SessionEvent) => {
    switch (event.type) {
      case 'state':
        if (event.detail === 'connecting' || event.detail === 'waiting') {
          setOverlay({kind: 'connecting'});
        } else if (event.detail === 'connected') {
          setOverlay(null);
        } else if (event.detail === 'disconnected') {
          setOverlay(current => (current?.kind === 'error' ? current : {kind: 'disconnected'}));
        }
        break;
      case 'error':
        setOverlay({kind: 'error', title: 'Connection error', detail: `${event.detail.message} (${event.detail.name})`});
        break;
      case 'name':
        // guacd reports the protocol's own name ("vnc"); the configured
        // connection name (loaded below) is more useful.
        break;
      case 'clipboard':
        setClipboard(event.detail);
        navigator.clipboard?.writeText(event.detail).catch(() => {});
        break;
      case 'required':
        setCredentials({});
        setRequired(event.detail);
        break;
      case 'unstable':
        setUnstable(event.detail);
        break;
      case 'stats':
        reporterRef.current?.add(event.detail);
        if (showStatsRef.current) setStats(event.detail);
        break;
    }
  }, []);

  const connect = useCallback(async () => {
    const client = clientRef.current;
    if (!client) return;
    setOverlay({kind: 'connecting'});
    setRequired(null);
    reporterRef.current?.stop();
    reporterRef.current = null;
    const reporting = await loadReporter().catch(() => null);
    if (reporting && (await reporting.clientReportsEnabled())) {
      reporterRef.current = new reporting.SessionReporter(connectionId, client.mode);
    }
    let url = client.tunnel_url(tunnelBase(), connectionId);
    if (reporterRef.current) url += `&session=${encodeURIComponent(reporterRef.current.session)}`;
    client.connect(url);
  }, [connectionId]);

  useEffect(() => {
    let cancelled = false;
    api.connections().then(
      list => {
        const connection = list.find(c => c.id === connectionId);
        if (cancelled || !connection) return;
        setName(connection.name);
        document.title = `${connection.name} · molcajete`;
      },
      () => {},
    );
    return () => {
      cancelled = true;
    };
  }, [connectionId]);

  // Create the client once for this page and tear it down when leaving.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const {MolcajeteSession} = await loadClient();
        const client = await MolcajeteSession.create(displayRef.current!, {
          worker: forceMainThread ? false : undefined,
        });
        if (cancelled) {
          client.free();
          return;
        }
        clientRef.current = client;
        client.set_event_handler((type, detail) => handleEvent({type, detail} as SessionEvent));
        client.set_scale_mode('fit');
        await connect();
      } catch (err) {
        if (!cancelled) {
          setOverlay({
            kind: 'error',
            title: 'The client could not start',
            detail: err instanceof Error ? err.message : String(err),
          });
        }
      }
    })();
    const flush = () => reporterRef.current?.flush({beacon: true});
    window.addEventListener('pagehide', flush);
    return () => {
      cancelled = true;
      window.removeEventListener('pagehide', flush);
      reporterRef.current?.stop();
      reporterRef.current = null;
      clientRef.current?.free();
      clientRef.current = null;
      document.title = 'molcajete';
    };
  }, [connect, handleEvent]);

  const toggleMenu = useCallback((open?: boolean) => {
    setIsMenuOpen(current => {
      const next = open ?? !current;
      if (next) clientRef.current?.release_keys();
      else queueMicrotask(() => clientRef.current?.focus());
      return next;
    });
  }, []);

  // Ctrl+Alt+Shift toggles the menu.
  useEffect(() => {
    const held = new Set<string>();
    const down = (e: KeyboardEvent) => {
      held.add(e.key);
      if (held.has('Control') && held.has('Alt') && held.has('Shift')) {
        held.clear();
        toggleMenu();
      }
    };
    const up = (e: KeyboardEvent) => held.delete(e.key);
    const clear = () => held.clear();
    document.addEventListener('keydown', down, true);
    document.addEventListener('keyup', up, true);
    window.addEventListener('blur', clear);
    return () => {
      document.removeEventListener('keydown', down, true);
      document.removeEventListener('keyup', up, true);
      window.removeEventListener('blur', clear);
    };
  }, [toggleMenu]);

  const leave = () => navigate(href.connections);

  const submitCredentials = (event: FormEvent) => {
    event.preventDefault();
    for (const parameter of required ?? []) {
      clientRef.current?.send_argv(parameter, credentials[parameter] ?? '');
    }
    setRequired(null);
    clientRef.current?.focus();
  };

  return (
    <>
      <div ref={displayRef} {...stylex.props(styles.display)} />

      {overlay && (
        <Center axis="both" padding={4} xstyle={styles.overlay}>
          <Card padding={6} xstyle={styles.overlayCard}>
            <VStack gap={4} hAlign="center">
              {overlay.kind === 'connecting' && <Spinner size="xl" />}
              <VStack gap={1} hAlign="center">
                <Heading level={1} justify="center">
                  {overlay.kind === 'connecting'
                    ? 'Connecting…'
                    : overlay.kind === 'error'
                      ? overlay.title
                      : 'Disconnected'}
                </Heading>
                <Text color="secondary" justify="center">
                  {overlay.kind === 'error' ? overlay.detail : name}
                </Text>
              </VStack>
              <HStack gap={2} hAlign="center" wrap="wrap">
                {overlay.kind !== 'connecting' && <Button label="Reconnect" variant="primary" onClick={connect} />}
                <Button label="Back to connections" variant="secondary" onClick={leave} />
              </HStack>
            </VStack>
          </Card>
        </Center>
      )}

      {unstable && !overlay && (
        <Card padding={2} elevation="med" xstyle={styles.unstable}>
          <HStack gap={2} vAlign="center">
            <StatusDot variant="warning" label="Unstable" isPulsing />
            <Text>Connection unstable</Text>
          </HStack>
        </Card>
      )}

      {showStats && stats && (
        <Card padding={2} elevation="med" xstyle={styles.stats}>
          <Text type="code">{formatStats(stats, clientRef.current?.mode ?? '?')}</Text>
        </Card>
      )}

      <Dialog isOpen={required !== null} onOpenChange={() => {}} purpose="required" width={420}>
        <form onSubmit={submitCredentials}>
          <Layout
            height="auto"
            header={<DialogHeader title="Credentials required" subtitle={name} />}
            content={
              <LayoutContent>
                <VStack gap={4}>
                  {(required ?? []).map((parameter, i) => (
                    <TextInput
                      key={parameter}
                      label={parameter}
                      type={isSecretParameter(parameter) ? 'password' : 'text'}
                      value={credentials[parameter] ?? ''}
                      onChange={value => setCredentials(c => ({...c, [parameter]: value}))}
                      autoComplete="off"
                      autoCapitalize="none"
                      hasAutoFocus={i === 0}
                    />
                  ))}
                </VStack>
              </LayoutContent>
            }
            footer={
              <LayoutFooter>
                <HStack gap={2} hAlign="end">
                  <Button label="Cancel" variant="secondary" onClick={leave} />
                  <Button label="Continue" variant="primary" type="submit" />
                </HStack>
              </LayoutFooter>
            }
          />
        </form>
      </Dialog>

      <Drawer isOpen={isMenuOpen} onOpenChange={open => toggleMenu(open)} label="Session menu" width={360}>
        <Layout
          header={
            <DrawerHeader
              title={name}
              subtitle={
                <HStack gap={1} vAlign="center">
                  <Kbd keys="ctrl+alt+shift" />
                  <Text color="secondary">to close</Text>
                </HStack>
              }
              onOpenChange={open => toggleMenu(open)}
            />
          }
          content={
            <LayoutContent>
              <VStack gap={5}>
                <TextArea
                  label="Clipboard"
                  description="Text here is sent to the remote clipboard; copies on the remote side show up here."
                  value={clipboard}
                  onChange={value => {
                    setClipboard(value);
                    clientRef.current?.send_clipboard(value);
                  }}
                  rows={5}
                  hasSpellCheck={false}
                />
                <Selector
                  label="Display scale"
                  value={scaleMode}
                  onChange={value => {
                    const mode = (value ?? 'fit') as ScaleMode;
                    setScaleMode(mode);
                    clientRef.current?.set_scale_mode(mode);
                  }}
                  options={[
                    {value: 'fit', label: 'Fit to window'},
                    {value: 'native', label: 'Native (1:1)'},
                  ]}
                />
                <Switch
                  label="Show performance stats"
                  value={showStats}
                  onChange={checked => {
                    setShowStats(checked);
                    if (!checked) setStats(null);
                  }}
                />
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack gap={2} hAlign="between">
                <Button label="Ctrl+Alt+Del" variant="secondary" onClick={() => clientRef.current?.send_ctrl_alt_del()} />
                <Button label="Disconnect" variant="destructive" onClick={leave} />
              </HStack>
            </LayoutFooter>
          }
        />
      </Drawer>
    </>
  );
}
