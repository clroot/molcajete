// Full-screen remote session: the wasm client renders into the display;
// connection states show over it, and Ctrl+Alt+Shift opens the session menu.
import {useCallback, useEffect, useRef, useState} from 'react';
import * as stylex from '@stylexjs/stylex';
import {href, navigate} from '../route';
import type {SessionEvent} from './client';
import {useSessionEvents} from './useSessionEvents';
import {useMolcajeteClient} from './useMolcajeteClient';
import {useMenuShortcut} from './useMenuShortcut';
import {useConnection} from './useConnection';
import {useRemoteComposition} from './useRemoteComposition';
import {effectiveDisplay, readDisplayChoices, tunnelOverrides, useDisplayChoices} from './displaySettings';
import {screenDpi} from './displayScale';
import StatusOverlay from './StatusOverlay';
import CredentialsDialog from './CredentialsDialog';
import SessionMenu, {type ScaleMode} from './SessionMenu';
import SessionMenuButton from './SessionMenuButton';
import SessionGuide from './SessionGuide';
import {readPreference, writePreference} from './storage';
import {StatsPanel, UnstableNotice} from './SessionBadges';

const styles = stylex.create({
  display: {position: 'fixed', inset: 0, backgroundColor: 'black', overflow: 'hidden'},
});

const leave = () => navigate(href.connections);

/** Protocols whose remote side runs its own input method. SSH is a terminal drawn by the daemon. */
const hasRemoteIme = (protocol: string | null) => protocol === 'rdp' || protocol === 'vnc';
/** Protocols whose scale and resizing the session menu changes. */
const hasRemoteDisplay = (protocol: string | null) => protocol === 'rdp';

export default function SessionPage({connectionId}: {connectionId: string}) {
  const displayRef = useRef<HTMLDivElement>(null);
  const [session, dispatch] = useSessionEvents();
  const {name: connectionName, protocol, display: connectionDisplay} = useConnection(connectionId);
  const [remoteComposition, setRemoteComposition] = useRemoteComposition(connectionId);
  const displayChoices = useDisplayChoices(connectionId);
  const display = effectiveDisplay(displayChoices.choices, connectionDisplay);
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const [scaleMode, setScaleMode] = useState<ScaleMode>('fit');
  const [showStats, setShowStats] = useState(false);
  const [hasSeenGuide, setHasSeenGuide] = useState(() => readPreference('sessionGuideSeen', false));

  const onEvent = useCallback(
    (event: SessionEvent) => {
      dispatch(event);
      // Copies on the remote side land in the local clipboard as well.
      if (event.type === 'clipboard') navigator.clipboard?.writeText(event.detail).catch(() => {});
    },
    [dispatch],
  );
  const onStartFailed = useCallback(
    (detail: string) => dispatch({type: 'failed', title: 'The client could not start', detail}),
    [dispatch],
  );
  // Read at every connect, so a reconnect starts with the latest choices.
  const tunnelParameters = useCallback(
    () => tunnelOverrides(readDisplayChoices(connectionId)),
    [connectionId],
  );
  const {client, mode, connect} = useMolcajeteClient({
    displayRef,
    connectionId,
    tunnelParameters,
    onEvent,
    onStartFailed,
  });

  // `mode` is set once the client exists.
  const composesRemotely = remoteComposition && hasRemoteIme(protocol);
  useEffect(() => {
    if (mode) client()?.set_remote_composition(composesRemotely);
  }, [mode, client, composesRemotely]);

  // The window's size only goes to servers that follow it.
  const followsWindow = hasRemoteDisplay(protocol) ? display.resize : true;
  useEffect(() => {
    if (mode) client()?.set_auto_resize(followsWindow);
  }, [mode, client, followsWindow]);

  const reconnect = () => {
    dispatch({type: 'reconnecting'});
    connect();
  };

  const setMenuOpen = useCallback(
    (open: boolean) => {
      setIsMenuOpen(open);
      // Keys held while the menu opens must not stay pressed remotely.
      if (open) client()?.release_keys();
      else client()?.focus();
    },
    [client],
  );
  const toggleMenu = useCallback(() => setMenuOpen(!isMenuOpen), [isMenuOpen, setMenuOpen]);
  useMenuShortcut(toggleMenu);

  const dismissGuide = () => {
    setHasSeenGuide(true);
    writePreference('sessionGuideSeen', true);
    client()?.focus();
  };

  const {status} = session;
  const isConnected = status.kind === 'connected';
  return (
    <>
      <div ref={displayRef} {...stylex.props(styles.display)} />

      {status.kind !== 'connected' && (
        <StatusOverlay
          status={status}
          connectionName={connectionName}
          onReconnect={reconnect}
          onLeave={leave}
        />
      )}
      {status.kind === 'connected' && session.isUnstable && <UnstableNotice />}
      {showStats && session.stats && <StatsPanel stats={session.stats} mode={mode ?? '?'} />}

      {isConnected && <SessionMenuButton onOpen={() => setMenuOpen(true)} />}
      {isConnected && !hasSeenGuide && !session.required && !isMenuOpen && (
        <SessionGuide onDismiss={dismissGuide} />
      )}

      {session.required && (
        <CredentialsDialog
          parameters={session.required}
          connectionName={connectionName}
          onCancel={leave}
          onSubmit={values => {
            for (const [name, value] of Object.entries(values)) client()?.send_argv(name, value);
            dispatch({type: 'answered'});
            client()?.focus();
          }}
        />
      )}

      <SessionMenu
        isOpen={isMenuOpen}
        onOpenChange={setMenuOpen}
        connectionName={connectionName}
        clipboard={session.clipboard}
        onClipboardChange={text => {
          dispatch({type: 'clipboard', detail: text});
          client()?.send_clipboard(text);
        }}
        scaleMode={scaleMode}
        onScaleModeChange={mode => {
          setScaleMode(mode);
          client()?.set_scale_mode(mode);
        }}
        remoteDisplay={hasRemoteDisplay(protocol) ? {...display, canChange: session.canChangeDisplay} : null}
        onRemoteScaleChange={scale => {
          displayChoices.setScale(scale);
          client()?.send_argv('dpi', String(scale === 'auto' ? screenDpi() : scale));
        }}
        onResizeToWindowChange={resize => {
          displayChoices.setResize(resize);
          // Before the size the effect sends, so that the server applies it.
          client()?.send_argv('resize-method', resize ? 'display-update' : '');
        }}
        showStats={showStats}
        onShowStatsChange={setShowStats}
        remoteComposition={hasRemoteIme(protocol) ? remoteComposition : null}
        onRemoteCompositionChange={setRemoteComposition}
        onCtrlAltDel={() => client()?.send_ctrl_alt_del()}
        onDisconnect={leave}
      />
    </>
  );
}
