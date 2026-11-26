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

type RdpModule = typeof import('@devolutions/iron-remote-desktop-rdp');

const styles = stylex.create({
  frame: {position: 'fixed', inset: 0, backgroundColor: 'black'},
  display: {display: 'block', width: '100%', height: '100%'},
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
  const elementRef = useRef<HTMLElement>(null);

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

  // The component announces itself once its canvas is ready.
  useEffect(() => {
    const element = elementRef.current;
    if (!rdp || !element) return;
    const onReady = (event: Event) =>
      setUi((event as CustomEvent<{irgUserInteraction: UserInteraction}>).detail.irgUserInteraction);
    element.addEventListener('ready', onReady);
    return () => element.removeEventListener('ready', onReady);
  }, [rdp]);

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
  }, [rdp, ui, connectionId, attempt]);

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
      <div {...stylex.props(styles.frame)}>
        {rdp && (
          <iron-remote-desktop
            ref={elementRef}
            scale="fit"
            flexcenter="true"
            module={rdp.Backend}
            {...stylex.props(styles.display)}
          />
        )}
      </div>
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
