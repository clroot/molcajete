// Full-screen remote session: the wasm client renders into the display;
// connection states show over it, and Ctrl+Alt+Shift opens the session menu.
import {useCallback, useRef, useState} from 'react';
import * as stylex from '@stylexjs/stylex';
import {href, navigate} from '../route';
import type {SessionEvent} from './client';
import {useSessionEvents} from './useSessionEvents';
import {useMolcajeteClient} from './useMolcajeteClient';
import {useMenuShortcut} from './useMenuShortcut';
import {useConnectionName} from './useConnectionName';
import StatusOverlay from './StatusOverlay';
import CredentialsDialog from './CredentialsDialog';
import SessionMenu, {type ScaleMode} from './SessionMenu';
import {StatsPanel, UnstableNotice} from './SessionBadges';

const styles = stylex.create({
  display: {position: 'fixed', inset: 0, backgroundColor: 'black', overflow: 'hidden'},
});

const leave = () => navigate(href.connections);

export default function SessionPage({connectionId}: {connectionId: string}) {
  const displayRef = useRef<HTMLDivElement>(null);
  const [session, dispatch] = useSessionEvents();
  const connectionName = useConnectionName(connectionId);
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const [scaleMode, setScaleMode] = useState<ScaleMode>('fit');
  const [showStats, setShowStats] = useState(false);

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
  const {client, mode, connect} = useMolcajeteClient({displayRef, connectionId, onEvent, onStartFailed});

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

  const {status} = session;
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
        showStats={showStats}
        onShowStatsChange={setShowStats}
        onCtrlAltDel={() => client()?.send_ctrl_alt_del()}
        onDisconnect={leave}
      />
    </>
  );
}
