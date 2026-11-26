// Full-screen remote session: the wasm client renders into the display;
// connection states show over it, and Ctrl+Alt+Shift opens the session menu.
import {useCallback, useEffect, useRef, useState} from 'react';
import {useToast} from '@astryxdesign/core/Toast';
import * as stylex from '@stylexjs/stylex';
import {href, navigate} from '../route';
import type {SessionEvent} from './client';
import {useSessionEvents} from './useSessionEvents';
import {useMolcajeteClient} from './useMolcajeteClient';
import {useMenuShortcut} from './useMenuShortcut';
import {useConnection, type ConnectionInfo} from './useConnection';
import {useRemoteComposition} from './useRemoteComposition';
import {effectiveDisplay, readDisplayChoices, tunnelOverrides, useDisplayChoices} from './displaySettings';
import {screenDpi} from './displayScale';
import StatusOverlay from './StatusOverlay';
import CredentialsDialog from './CredentialsDialog';
import SessionMenu, {type ScaleMode} from './SessionMenu';
import SessionMenuButton from './SessionMenuButton';
import SessionGuide from './SessionGuide';
import {readPreference, writePreference} from './storage';
import {
  AudioHint,
  CornerBadges,
  LinkStatus,
  ReadOnlyBadge,
  RecordingBadge,
  StatsPanel,
  UnstableNotice,
  WatchersNotice,
} from './SessionBadges';
import SharingSection from './SharingSection';
import RecordingSection from './RecordingSection';
import {newPageId, useSharedSession, type Participant} from './sharing';
import {unlockAudio} from './audio';
import {hasLinks, isTerminal} from './terminal';
import {useCopyFeedback} from './useCopyFeedback';
import {transferStatus, useFiles} from './files';
import FilesDrawer from './FilesDrawer';
import {ActiveTransfers} from './TransferList';
import DropOverlay from './DropOverlay';
import {useFileDrop} from './useFileDrop';

const styles = stylex.create({
  display: {position: 'fixed', inset: 0, backgroundColor: 'black', overflow: 'hidden'},
});

const leave = () => navigate(href.connections);

/** Protocols whose remote side runs its own input method. SSH is a terminal drawn by the daemon. */
const hasRemoteIme = (protocol: string | null) => protocol === 'rdp' || protocol === 'vnc';
/** Protocols whose scale and resizing the session menu changes. */
const hasRemoteDisplay = (protocol: string | null) => protocol === 'rdp';

type Props = {
  connectionId: string;
  /**
   * Opened by a one-time link: the ticket goes into the tunnel URL instead
   * of the session cookie, and there is no connection list to go back to
   * and no reconnecting (the ticket is used up).
   */
  ticket?: {ticket: string; connection: ConnectionInfo};
};

export default function SessionPage({connectionId, ticket}: Props) {
  const displayRef = useRef<HTMLDivElement>(null);
  const [session, dispatch] = useSessionEvents();
  const [files, dispatchFiles] = useFiles();
  const showToast = useToast();
  const [isFilesOpen, setIsFilesOpen] = useState(false);
  const {
    name: connectionName,
    protocol,
    display: connectionDisplay,
    restrictions,
  } = useConnection(connectionId, ticket?.connection);
  const ticketId = ticket?.ticket;
  const [remoteComposition, setRemoteComposition] = useRemoteComposition(connectionId);
  const displayChoices = useDisplayChoices(connectionId);
  const display = effectiveDisplay(displayChoices.choices, connectionDisplay);
  const [isMenuOpen, setIsMenuOpen] = useState(false);
  const [scaleMode, setScaleMode] = useState<ScaleMode>('fit');
  const [showStats, setShowStats] = useState(false);
  const [hasSeenGuide, setHasSeenGuide] = useState(() => readPreference('sessionGuideSeen', false));
  const [hasSeenTerminalGuide, setHasSeenTerminalGuide] = useState(() =>
    readPreference('terminalGuideSeen', false),
  );
  const terminal = isTerminal(protocol);
  const onCopyEvent = useCopyFeedback(terminal);

  const onEvent = useCallback(
    (event: SessionEvent) => {
      dispatch(event);
      dispatchFiles(event);
      if (event.type === 'transfer' && (event.detail.state === 'done' || event.detail.state === 'failed')) {
        const t = event.detail;
        showToast({
          body: `${t.name}: ${transferStatus(t)}`,
          type: t.state === 'failed' ? 'error' : undefined,
        });
      }
      // Copies on the remote side land in the local clipboard as well.
      onCopyEvent(event);
    },
    [dispatch, dispatchFiles, onCopyEvent, showToast],
  );
  const onStartFailed = useCallback(
    (detail: string) => dispatch({type: 'failed', title: 'The client could not start', detail}),
    [dispatch],
  );
  // Names this page's tunnel for sharing it; a new one for every connect.
  const [pageId, setPageId] = useState(newPageId);
  // Read at every connect, so a reconnect starts with the latest choices.
  const tunnelParameters = useCallback(() => {
    const page = newPageId();
    setPageId(page);
    return {
      ...tunnelOverrides(readDisplayChoices(connectionId)),
      ...(ticketId === undefined ? {} : {ticket: ticketId}),
      page,
    };
  }, [connectionId, ticketId]);
  const {client, mode, connect} = useMolcajeteClient({
    displayRef,
    connectionId,
    tunnelParameters,
    canReport: ticketId === undefined,
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

  useEffect(() => {
    if (mode) client()?.set_terminal_shortcuts(terminal);
  }, [mode, client, terminal]);

  const reconnect = () => {
    unlockAudio();
    dispatch({type: 'reconnecting'});
    connect();
  };
  const isTicket = ticketId !== undefined;

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
    if (terminal) {
      setHasSeenTerminalGuide(true);
      writePreference('terminalGuideSeen', true);
    }
    client()?.focus();
  };
  // Shown once, and once more for the first terminal after that.
  const isGuideDue = !hasSeenGuide || (terminal && !hasSeenTerminalGuide);

  const {status} = session;
  const isConnected = status.kind === 'connected';
  // A session opened by a one-time link has no signed-in user to share it.
  const {shared, reload: reloadShared} = useSharedSession(pageId, isConnected && ticketId === undefined);
  const participants = shared?.participants;
  useJoinToasts(participants);
  const administrators = (participants ?? []).filter(p => p.kind === 'shadow').map(p => p.username);

  // Files dropped on the session go to the server's upload folder.
  const dropTarget = isConnected ? files.filesystems.find(f => f.canUpload) : undefined;
  const isDraggingFiles = useFileDrop(dropTarget !== undefined, dropped => {
    for (const file of dropped) client()?.upload(file);
  });
  const openFiles = () => {
    setMenuOpen(false);
    setIsFilesOpen(true);
  };
  return (
    <>
      <div ref={displayRef} {...stylex.props(styles.display)} />

      {status.kind !== 'connected' && (
        <StatusOverlay
          status={status}
          connectionName={connectionName}
          onReconnect={isTicket ? undefined : reconnect}
          onLeave={isTicket ? undefined : leave}
        />
      )}
      {status.kind === 'connected' && session.isUnstable && <UnstableNotice />}
      {showStats && session.stats && <StatsPanel stats={session.stats} mode={mode ?? '?'} />}
      {isConnected && terminal && session.link && <LinkStatus url={session.link} />}
      {isConnected && session.isAudioBlocked && <AudioHint />}
      <CornerBadges>
        {isConnected && restrictions.readOnly && <ReadOnlyBadge />}
        {shared?.recording.active && <RecordingBadge />}
        {participants && participants.length > 0 && (
          <WatchersNotice administrators={administrators} others={participants.length} />
        )}
      </CornerBadges>
      {!isFilesOpen && (
        <ActiveTransfers transfers={files.transfers} onCancel={id => client()?.cancel_transfer(id)} />
      )}
      {isDraggingFiles && dropTarget && (
        <DropOverlay target={protocol === 'ssh' ? 'your home folder' : `the ${dropTarget.name} drive`} />
      )}

      {isConnected && <SessionMenuButton onOpen={() => setMenuOpen(true)} />}
      {isConnected && isGuideDue && !session.required && !isMenuOpen && (
        <SessionGuide terminal={terminal ? {hasLinks: hasLinks(protocol)} : null} onDismiss={dismissGuide} />
      )}

      {session.required && (
        <CredentialsDialog
          parameters={session.required}
          connectionName={connectionName}
          onCancel={isTicket ? () => client()?.disconnect() : leave}
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
        isTerminal={terminal}
        restrictions={restrictions}
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
        onDisconnect={
          isTicket
            ? () => {
                setMenuOpen(false);
                client()?.disconnect();
              }
            : leave
        }
        recording={
          shared && <RecordingSection pageId={pageId} recording={shared.recording} onChange={reloadShared} />
        }
        sharing={shared && <SharingSection pageId={pageId} shared={shared} onChange={reloadShared} />}
        onOpenFiles={files.filesystems.length > 0 ? openFiles : null}
      />

      <FilesDrawer
        isOpen={isFilesOpen}
        onOpenChange={open => {
          setIsFilesOpen(open);
          if (!open) client()?.focus();
        }}
        filesystems={files.filesystems}
        transfers={files.transfers}
        client={client}
      />
    </>
  );
}

/** Says who joined or left the session since the last check. */
function useJoinToasts(participants: Participant[] | undefined) {
  const showToast = useToast();
  const previous = useRef<Participant[] | null>(null);
  useEffect(() => {
    if (!participants) {
      previous.current = null;
      return;
    }
    const before = previous.current;
    previous.current = participants;
    if (before === null) return;
    const ids = new Set(before.map(p => p.id));
    for (const p of participants.filter(p => !ids.has(p.id))) {
      showToast({
        body:
          p.kind === 'shadow'
            ? `${p.username}, an administrator, is watching this session.`
            : `${p.username} joined this session${p.readOnly ? ' to watch' : ''}.`,
      });
    }
    const now = new Set(participants.map(p => p.id));
    for (const p of before.filter(p => !now.has(p.id))) {
      showToast({body: `${p.username} left this session.`});
    }
  }, [participants, showToast]);
}
