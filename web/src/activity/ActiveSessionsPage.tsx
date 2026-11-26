import {useEffect, useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {Button} from '@astryxdesign/core/Button';
import {Text} from '@astryxdesign/core/Text';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {useToast} from '@astryxdesign/core/Toast';
import {SignalIcon} from '@heroicons/react/24/outline';
import {errorMessage, type User} from '../api';
import {useApiData} from '../useApiData';
import {href} from '../route';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import {protocolLabel} from '../connections/protocols';
import {formatClock, formatSize} from '../recordings/format';
import RecordingLinks from '../recordings/RecordingLinks';
import {sessionsApi, type LiveSession} from './api';

/** How often the list refreshes while the page is visible. */
const REFRESH_MS = 3000;

/** Calls `reload` every few seconds while the page is visible. */
function useRefresh(reload: () => void) {
  useEffect(() => {
    const timer = setInterval(() => {
      if (document.visibilityState === 'visible') reload();
    }, REFRESH_MS);
    return () => clearInterval(timer);
  }, [reload]);
}

/** Who is connected to what right now, for administrators. */
export default function ActiveSessionsPage({currentUser}: {currentUser: User}) {
  const sessions = useApiData(sessionsApi.live);
  const {reload} = sessions;
  useRefresh(reload);
  const [ending, setEnding] = useState<LiveSession | 'all' | null>(null);
  const [isEnding, setIsEnding] = useState(false);
  const showToast = useToast();

  const others =
    sessions.status === 'ready' ? sessions.data.filter(s => s.userId !== currentUser.id).length : 0;

  const confirm = async () => {
    if (ending === null) return;
    setIsEnding(true);
    try {
      if (ending === 'all') {
        const {ended} = await sessionsApi.endAll();
        showToast({body: ended === 1 ? 'Ended 1 session.' : `Ended ${ended} sessions.`});
      } else {
        await sessionsApi.end(ending.id);
        showToast({body: `Ended ${ending.username}'s session.`});
      }
      setEnding(null);
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    } finally {
      setIsEnding(false);
      reload();
    }
  };

  const owners = new Map(sessions.status === 'ready' ? sessions.data.map(s => [s.id, s.username]) : []);
  const joinedAs = (s: LiveSession) => {
    const owner = (s.joinedTo && owners.get(s.joinedTo)) ?? 'another user';
    if (s.joinKind === 'shadow') return `Watching ${owner}`;
    return `${s.readOnly ? 'Watching' : 'Controlling'} ${owner} (link)`;
  };

  const columns: TableColumn<LiveSession>[] = [
    {
      key: 'username',
      header: 'User',
      renderCell: s => (
        <VStack gap={0.5}>
          <Text weight="medium">{s.userId === currentUser.id ? `${s.username} (you)` : s.username}</Text>
          {s.joinKind && <Text color="secondary">{joinedAs(s)}</Text>}
          {s.peer && <Text color="secondary">{s.peer}</Text>}
        </VStack>
      ),
    },
    {
      key: 'connectionName',
      header: 'Connection',
      renderCell: s => (
        <VStack gap={0.5}>
          <Text>{s.connectionName}</Text>
          <Text color="secondary">{protocolLabel(s.protocol)}</Text>
        </VStack>
      ),
    },
    {
      key: 'startedAt',
      header: 'Started',
      width: pixel(190),
      renderCell: s => (
        <VStack gap={0.5}>
          <Timestamp value={s.startedAt} format="date_time" color="primary" type="body" />
          <Text color="secondary">{formatClock(Date.now() - s.startedAt)}</Text>
        </VStack>
      ),
    },
    {
      key: 'bytesToBrowser',
      header: 'Traffic',
      width: pixel(150),
      renderCell: s => (
        <VStack gap={0.5}>
          <Text>{formatSize(s.bytesToBrowser)} received</Text>
          <Text color="secondary">{formatSize(s.bytesToGuacd)} sent</Text>
        </VStack>
      ),
    },
    {
      key: 'rttMs',
      header: 'Round trip',
      align: 'end',
      width: pixel(110),
      renderCell: s => (
        <Text color={s.rttMs === null ? 'secondary' : 'primary'}>
          {s.rttMs === null ? '—' : `${s.rttMs} ms`}
        </Text>
      ),
    },
    {
      key: 'recordingIds',
      header: 'Recording',
      width: pixel(130),
      renderCell: s =>
        s.recording ? (
          <VStack gap={0.5}>
            <HStack gap={2} vAlign="center">
              <StatusDot variant="error" label="Recording" isPulsing />
              <Text>Recording</Text>
            </HStack>
            <RecordingLinks ids={s.recordingIds} />
          </VStack>
        ) : (
          <RecordingLinks ids={s.recordingIds} />
        ),
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(160),
      renderCell: s => (
        <HStack hAlign="end" gap={2} vAlign="center">
          {s.joinKind === null && s.userId !== currentUser.id && (
            <Button
              label="Watch"
              size="sm"
              variant="secondary"
              href={href.watch(s.id)}
              tooltip="Watch this session without controlling it. The user sees that you are watching."
            />
          )}
          <Button label="End" size="sm" variant="destructive" onClick={() => setEnding(s)} />
        </HStack>
      ),
    },
  ];

  return (
    <PageFrame
      title="Active sessions"
      description="Who is connected to what right now. The list refreshes every few seconds."
      action={
        <Button
          label="End all"
          variant="destructive"
          isDisabled={others === 0}
          tooltip={others === 0 ? 'No sessions of other users are open.' : undefined}
          onClick={() => setEnding('all')}
        />
      }>
      <AsyncContent data={sessions} errorTitle="Could not load the active sessions">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            verticalAlign="top"
            emptyState={
              <EmptyState
                icon={<SignalIcon width={32} height={32} />}
                title="No active sessions"
                description="Sessions show up here while someone is connected."
              />
            }
          />
        )}
      </AsyncContent>

      <AlertDialog
        isOpen={ending !== null}
        onOpenChange={open => !open && setEnding(null)}
        title={
          ending === 'all'
            ? 'End every session?'
            : ending
              ? `End ${ending.username}'s session on ${ending.connectionName}?`
              : ''
        }
        description={
          ending === 'all'
            ? 'Everyone else is disconnected and told that an administrator ended the session. Your own sessions stay open.'
            : 'The user is disconnected and told that an administrator ended the session. They can connect again.'
        }
        actionLabel={ending === 'all' ? 'End all' : 'End session'}
        isActionLoading={isEnding}
        onAction={confirm}
      />
    </PageFrame>
  );
}
