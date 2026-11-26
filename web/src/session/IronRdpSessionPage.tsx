// A full-screen RDP session run by IronRDP's web client: RDP in the
// browser, with molcajeted only passing the connection through. The
// connection's stored password stays on the server; the page signs in to
// molcajeted with a one-time credential instead.
import {useCallback, useEffect, useRef, useState} from 'react';
import * as stylex from '@stylexjs/stylex';
import type {UserInteraction} from '@devolutions/iron-remote-desktop';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {api} from '../api';
import StatusOverlay from './StatusOverlay';
import SessionMenuButton from './SessionMenuButton';
import {useMenuShortcut} from './useMenuShortcut';
import type {ConnectionStatus} from './useSessionEvents';
import {describeIronRdpError, ironRdpTunnelUrl} from './ironrdp';
import {blockPageScroll, installIronRdpTouch} from './ironrdpTouch';

type RdpModule = typeof import('@devolutions/iron-remote-desktop-rdp');

const styles = stylex.create({
  // Touches belong to the remote screen, not to page scrolling or zooming.
  frame: {
    position: 'fixed',
    inset: 0,
    backgroundColor: 'black',
    touchAction: 'none',
    overscrollBehavior: 'none',
  },
  display: {display: 'block', width: '100%', height: '100%', touchAction: 'none'},
});

/** IronRDP's packages, loaded once and only for these sessions (several MB). */
let loading: Promise<RdpModule> | null = null;
function loadIronRdp(): Promise<RdpModule> {
  loading ??= Promise.all([
    import('@devolutions/iron-remote-desktop'),
    import('@devolutions/iron-remote-desktop-rdp'),
  ]).then(async ([, rdp]) => {
    await rdp.init('WARN');
    return rdp;
  });
  loading.catch(() => (loading = null));
  return loading;
}

/** How long the web component may take to start before the page gives up. */
const READY_TIMEOUT_MS = 15_000;

/** Time without a new window size before the remote desktop follows it. */
const RESIZE_DELAY_MS = 300;

type Props = {
  connectionId: string;
  connectionName: string;
  onLeave: () => void;
  /** Reconnects with molcajete's client; absent when the connection does not allow switching. */
  onSwitchClient?: () => void;
};

export default function IronRdpSessionPage({connectionId, connectionName, onLeave, onSwitchClient}: Props) {
  const [rdp, setRdp] = useState<RdpModule | null>(null);
  const [ui, setUi] = useState<UserInteraction | null>(null);
  const [status, setStatus] = useState<ConnectionStatus>({kind: 'connecting'});
  const [attempt, setAttempt] = useState(0);
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const frameRef = useRef<HTMLDivElement>(null);

  // No scrolling or pull to reload of the page while the session is open.
  useEffect(() => blockPageScroll(), []);

  useEffect(() => {
    let isCurrent = true;
    loadIronRdp().then(
      module => isCurrent && setRdp(module),
      err =>
        isCurrent &&
        setStatus({
          kind: 'failed',
          title: 'The client could not start',
          detail: describeIronRdpError(err).detail,
        }),
    );
    return () => {
      isCurrent = false;
    };
  }, []);

  // The component announces itself with `ready` a few microtasks after it
  // is inserted, sooner than an effect would attach a listener to an element
  // React rendered (on Safari the page waited forever). So it is made here,
  // listened to, and only then inserted.
  useEffect(() => {
    const frame = frameRef.current;
    if (!rdp || !frame) return;
    const element = document.createElement('iron-remote-desktop') as HTMLElement & {module: unknown};
    element.setAttribute('scale', 'fit');
    element.setAttribute('flexcenter', 'true');
    element.className = stylex.props(styles.display).className ?? '';
    element.module = rdp.Backend;
    // Never a spinner forever: a component that does not start says so.
    const timer = setTimeout(
      () =>
        setStatus({
          kind: 'failed',
          title: 'The client could not start',
          detail: "IronRDP's client did not get ready. Reload the page, or reconnect with molcajete.",
        }),
      READY_TIMEOUT_MS,
    );
    element.addEventListener('ready', event => {
      clearTimeout(timer);
      setUi((event as CustomEvent<{irgUserInteraction: UserInteraction}>).detail.irgUserInteraction);
    });
    const removeTouch = installIronRdpTouch(element);
    frame.append(element);
    return () => {
      clearTimeout(timer);
      removeTouch();
      element.remove();
      setUi(null);
    };
    // Every attempt starts with a new component.
  }, [rdp, attempt]);

  useEffect(() => {
    if (!rdp || !ui) return;
    let isCurrent = true;
    const run = async () => {
      const grant = await api.ironrdpGrant(connectionId);
      const config = ui
        .configBuilder()
        .withUsername(grant.username)
        .withPassword(grant.password)
        // molcajeted connects to the connection's own host; this only names it.
        .withDestination(connectionId)
        .withProxyAddress(ironRdpTunnelUrl(location, connectionId, grant.token))
        .withAuthToken(grant.token)
        .withDesktopSize({width: window.innerWidth, height: window.innerHeight})
        .withExtension(rdp.displayControl(true))
        // Bitmap updates instead of the graphics pipeline: without an H.264
        // decoder the web client offers only version 8 of the pipeline, and
        // a Windows 11 host left that offer unanswered (a black screen).
        .withExtension(rdp.legacyGraphics(true))
        .build();
      const session = await ui.connect(config);
      if (!isCurrent) return;
      ui.setVisibility(true);
      setStatus({kind: 'connected'});
      await session.run();
      if (isCurrent) setStatus({kind: 'disconnected'});
    };
    run().catch(err => {
      if (isCurrent) setStatus({kind: 'failed', ...describeIronRdpError(err)});
    });
    return () => {
      isCurrent = false;
      ui.shutdown();
    };
  }, [rdp, ui, connectionId]);

  // The remote desktop follows the window.
  useEffect(() => {
    if (!ui || status.kind !== 'connected') return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const onResize = () => {
      clearTimeout(timer);
      timer = setTimeout(() => ui.resize(window.innerWidth, window.innerHeight), RESIZE_DELAY_MS);
    };
    window.addEventListener('resize', onResize);
    return () => {
      clearTimeout(timer);
      window.removeEventListener('resize', onResize);
    };
  }, [ui, status.kind]);

  const toggleMenu = useCallback(() => setIsMenuOpen(open => !open), []);
  useMenuShortcut(toggleMenu);

  const reconnect = () => {
    setStatus({kind: 'connecting'});
    setAttempt(n => n + 1);
  };
  const isConnected = status.kind === 'connected';

  return (
    <>
      <div ref={frameRef} {...stylex.props(styles.frame)} />
      {status.kind !== 'connected' && (
        <StatusOverlay
          status={status}
          connectionName={connectionName}
          onReconnect={reconnect}
          onLeave={onLeave}
        />
      )}
      {isConnected && <SessionMenuButton onOpen={() => setIsMenuOpen(true)} />}
      <Drawer isOpen={isMenuOpen} onOpenChange={setIsMenuOpen} label="Session menu" width={360}>
        <Layout
          header={
            <DrawerHeader
              title={connectionName}
              subtitle={
                <HStack gap={1} vAlign="center">
                  <Kbd keys="ctrl+alt+shift" />
                  <Text color="secondary">to close</Text>
                </HStack>
              }
              onOpenChange={setIsMenuOpen}
            />
          }
          content={
            <LayoutContent>
              <VStack gap={4}>
                <Text color="secondary">
                  This session runs IronRDP in the browser. Recording, sharing, the file panel and the
                  performance stats come with molcajete&apos;s client.
                </Text>
                {onSwitchClient && (
                  <Button label="Reconnect with molcajete" variant="secondary" onClick={onSwitchClient} />
                )}
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack gap={2} hAlign="between">
                <Button
                  label="Ctrl+Alt+Del"
                  variant="secondary"
                  onClick={() => ui?.ctrlAltDel()}
                  isDisabled={!isConnected}
                />
                <Button label="Disconnect" variant="destructive" onClick={onLeave} />
              </HStack>
            </LayoutFooter>
          }
        />
      </Drawer>
    </>
  );
}
