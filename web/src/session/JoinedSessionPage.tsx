// A session someone else opened, joined through a share link (`#/join/<token>`)
// or watched by an administrator (`#/watch/<session id>`).
import {useCallback, useEffect, useRef, useState} from 'react';
import * as stylex from '@stylexjs/stylex';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Selector} from '@astryxdesign/core/Selector';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Button} from '@astryxdesign/core/Button';
import {Kbd} from '@astryxdesign/core/Kbd';
import {Text} from '@astryxdesign/core/Text';
import {useToast} from '@astryxdesign/core/Toast';
import {href, navigate} from '../route';
import {sessionsApi} from '../activity/api';
import type {SessionEvent} from './client';
import {useSessionEvents} from './useSessionEvents';
import {useMolcajeteClient} from './useMolcajeteClient';
import {useMenuShortcut} from './useMenuShortcut';
import StatusOverlay from './StatusOverlay';
import SessionMenuButton from './SessionMenuButton';
import {FullScreenVideoSwitch, type ScaleMode} from './SessionMenu';
import {CornerBadges, RecordingBadge} from './SessionBadges';
import {REFRESH_MS, sharingApi} from './sharing';

const styles = stylex.create({
  display: {position: 'fixed', inset: 0, backgroundColor: 'black', overflow: 'hidden'},
});

/** A share link's token, or the tunnel id of the session an administrator watches. */
export type JoinTarget = {kind: 'link' | 'watch'; id: string};

type Joined = {connectionName: string; owner: string; readOnly: boolean};

/** Statuses the gateway closes a joined tunnel with, as this page explains them. */
const ENDED: Record<number, {title: string; detail: string}> = {
  // RESOURCE_NOT_FOUND
  0x0204: {
    title: 'This session is not available',
    detail: 'The link has expired or was revoked, or the session has ended.',
  },
  // CLIENT_FORBIDDEN
  0x0303: {title: 'Access removed', detail: 'The link was revoked, or sharing is turned off.'},
  // SESSION_CLOSED
  0x020b: {title: 'Ended by an administrator', detail: 'An administrator ended this session.'},
};

const leave = () => navigate(href.connections);

function useJoined({kind, id}: JoinTarget) {
  const [joined, setJoined] = useState<Joined | null>(null);
  useEffect(() => {
    let isCurrent = true;
    const found =
      kind === 'link'
        ? sharingApi.describeLink(id).then(link => ({
            connectionName: link.connectionName,
            owner: link.owner,
            readOnly: link.readOnly,
          }))
        : sessionsApi.live().then(list => {
            const session = list.find(s => s.id === id);
            return session
              ? {connectionName: session.connectionName, owner: session.username, readOnly: true}
              : null;
          });
    found.then(
      value => isCurrent && setJoined(value),
      () => {}, // The tunnel reports what went wrong.
    );
    return () => {
      isCurrent = false;
    };
  }, [kind, id]);
  return joined;
}

/** Whether the session joined is being recorded, checked every few seconds while connected. */
function useIsRecorded({kind, id}: JoinTarget, isConnected: boolean): boolean {
  const [isRecorded, setIsRecorded] = useState(false);
  useEffect(() => {
    if (!isConnected) return;
    let isCurrent = true;
    const check = () => {
      const recorded =
        kind === 'link'
          ? sharingApi.describeLink(id).then(link => link.recording)
          : sessionsApi.live().then(list => list.find(s => s.id === id)?.recording ?? false);
      recorded.then(
        value => isCurrent && setIsRecorded(value),
        () => {}, // The tunnel reports a session that ended.
      );
    };
    check();
    const timer = setInterval(() => {
      if (document.visibilityState === 'visible') check();
    }, REFRESH_MS);
    return () => {
      isCurrent = false;
      clearInterval(timer);
    };
  }, [kind, id, isConnected]);
  return isConnected && isRecorded;
}

/** Tells once when the session turns out to be recorded: on joining, or when recording starts. */
function useRecordingNotice(isRecorded: boolean) {
  const showToast = useToast();
  const wasRecorded = useRef(false);
  useEffect(() => {
    if (isRecorded && !wasRecorded.current) {
      showToast({body: 'This session is being recorded.', autoHideDuration: 6000});
    }
    wasRecorded.current = isRecorded;
  }, [isRecorded, showToast]);
}

export default function JoinedSessionPage({kind, id}: JoinTarget) {
  const displayRef = useRef<HTMLDivElement>(null);
  const [session, dispatch] = useSessionEvents();
  const joined = useJoined({kind, id});
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const [scaleMode, setScaleMode] = useState<ScaleMode>('fit');

  const onEvent = useCallback(
    (event: SessionEvent) => {
      if (event.type === 'error' && event.detail.message.startsWith('tunnel closed')) {
        const ended = event.detail.code === undefined ? undefined : ENDED[event.detail.code];
        if (ended) {
          dispatch({type: 'failed', ...ended});
          return;
        }
      }
      dispatch(event);
    },
    [dispatch],
  );
  const onStartFailed = useCallback(
    (detail: string) => dispatch({type: 'failed', title: 'The client could not start', detail}),
    [dispatch],
  );
  const tunnelParameters = useCallback(
    (): Record<string, string> => (kind === 'link' ? {join: id} : {shadow: id}),
    [kind, id],
  );
  const {client, mode, connect} = useMolcajeteClient({
    displayRef,
    connectionId: '',
    tunnelParameters,
    onEvent,
    onStartFailed,
  });

  // The owner's window decides the size of the remote display.
  useEffect(() => {
    if (mode) client()?.set_auto_resize(false);
  }, [mode, client]);

  const setMenuOpen = useCallback(
    (open: boolean) => {
      setIsMenuOpen(open);
      if (open) client()?.release_keys();
      else client()?.focus();
    },
    [client],
  );
  const toggleMenu = useCallback(() => setMenuOpen(!isMenuOpen), [isMenuOpen, setMenuOpen]);
  useMenuShortcut(toggleMenu);

  const isConnected = session.status.kind === 'connected';
  const isRecorded = useIsRecorded({kind, id}, isConnected);
  useRecordingNotice(isRecorded);
  // The owner leaving closes the tunnel normally.
  const status =
    session.status.kind === 'disconnected'
      ? {kind: 'failed' as const, title: 'The session has ended', detail: 'Its owner disconnected.'}
      : session.status;
  const connectionName = joined?.connectionName ?? 'Shared session';
  const canControl = joined !== null && !joined.readOnly;
  const subtitle = joined
    ? `${joined.owner}'s session · ${joined.readOnly ? 'watching' : 'you can control it'}`
    : 'Shared session';

  return (
    <>
      <div ref={displayRef} {...stylex.props(styles.display)} />
      {status.kind !== 'connected' && (
        <StatusOverlay
          status={status}
          connectionName={connectionName}
          onReconnect={() => {
            dispatch({type: 'reconnecting'});
            connect();
          }}
          onLeave={leave}
        />
      )}
      <CornerBadges>{isRecorded && <RecordingBadge />}</CornerBadges>
      {isConnected && <SessionMenuButton onOpen={() => setMenuOpen(true)} />}

      <Drawer isOpen={isMenuOpen} onOpenChange={setMenuOpen} label="Session menu" width={360}>
        <Layout
          header={
            <DrawerHeader
              title={connectionName}
              subtitle={
                <VStack gap={1}>
                  <Text color="secondary">{subtitle}</Text>
                  <HStack gap={1} vAlign="center">
                    <Kbd keys="ctrl+alt+shift" />
                    <Text color="secondary">to close</Text>
                  </HStack>
                </VStack>
              }
              onOpenChange={setMenuOpen}
            />
          }
          content={
            <LayoutContent>
              <VStack gap={5}>
                {canControl && (
                  <TextArea
                    label="Clipboard"
                    description="Text here is sent to the remote clipboard."
                    value={session.clipboard}
                    onChange={text => {
                      dispatch({type: 'clipboard', detail: text});
                      client()?.send_clipboard(text);
                    }}
                    rows={5}
                    hasSpellCheck={false}
                  />
                )}
                <Selector
                  label="Fit"
                  description="How the picture is shown when its size differs from this window. The owner's window decides the size."
                  value={scaleMode}
                  onChange={value => {
                    const mode = (value ?? 'fit') as ScaleMode;
                    setScaleMode(mode);
                    client()?.set_scale_mode(mode);
                  }}
                  options={[
                    {value: 'fit', label: 'Fit to window'},
                    {value: 'native', label: 'Native (1:1)'},
                  ]}
                />
                {session.videoMode !== null && (
                  <FullScreenVideoSwitch
                    value={session.videoMode === 'full-screen'}
                    onChange={isFullScreen =>
                      client()?.send_argv('video-mode', isFullScreen ? 'full-screen' : 'auto')
                    }
                  />
                )}
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack gap={2} hAlign="end">
                <Button label="Leave" variant="destructive" onClick={leave} />
              </HStack>
            </LayoutFooter>
          }
        />
      </Drawer>
    </>
  );
}
