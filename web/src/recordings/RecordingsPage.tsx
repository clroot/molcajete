import {useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {useToast} from '@astryxdesign/core/Toast';
import {FilmIcon} from '@heroicons/react/24/outline';
import {api, recordingFileUrl, type Recording} from '../api';
import {useApiData} from '../useApiData';
import {href, navigate} from '../route';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import {protocolLabel} from '../connections/protocols';
import {formatClock, formatSize} from './format';

/**
 * Recorded sessions, newest first: every one for administrators, who also
 * delete them; for other users, those they recorded from the session menu.
 */
export default function RecordingsPage({isAdmin}: {isAdmin: boolean}) {
  const recordings = useApiData(api.recordings);
  const [deleting, setDeleting] = useState<Recording | null>(null);
  const showToast = useToast();

  const columns: TableColumn<Recording>[] = [
    {
      key: 'startedAt',
      header: 'Session',
      renderCell: r => (
        <VStack gap={0.5}>
          <Link href={href.playback(r.id)} weight="medium">
            {r.connectionName}
          </Link>
          <Timestamp value={r.startedAt} format="date_time" />
        </VStack>
      ),
    },
    {
      key: 'username',
      header: 'User',
      width: pixel(160),
      renderCell: r => <Text>{r.username}</Text>,
    },
    {
      key: 'protocol',
      header: 'Protocol',
      width: pixel(110),
      renderCell: r => <Badge label={protocolLabel(r.protocol)} />,
    },
    {
      key: 'endedAt',
      header: 'Length',
      width: pixel(150),
      renderCell: r =>
        r.endedAt === null ? (
          <HStack gap={2} vAlign="center">
            <StatusDot variant="accent" label="Recording" />
            <Text>Recording</Text>
          </HStack>
        ) : (
          <Text>{formatClock(r.endedAt - r.startedAt)}</Text>
        ),
    },
    {
      key: 'sizeBytes',
      header: 'Size',
      align: 'end',
      width: pixel(100),
      renderCell: r => <Text>{formatSize(r.sizeBytes)}</Text>,
    },
    {
      key: 'includeKeys',
      header: 'Contents',
      width: pixel(130),
      renderCell: r =>
        r.includeKeys ? (
          <HStack gap={2} vAlign="center">
            <StatusDot variant="warning" label="Includes key presses" />
            <Text>Key presses</Text>
          </HStack>
        ) : null,
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(80),
      renderCell: r => (
        <HStack hAlign="end">
          <MoreMenu
            size="sm"
            label={`Options for the recording of ${r.connectionName}`}
            items={[
              {label: 'Play', onClick: () => navigate(href.playback(r.id))},
              {label: 'Download', onClick: () => location.assign(recordingFileUrl(r.id, true))},
              // A session still being recorded cannot be deleted.
              ...(r.endedAt === null || !isAdmin ? [] : [{label: 'Delete', onClick: () => setDeleting(r)}]),
            ]}
          />
        </HStack>
      ),
    },
  ];

  return (
    <PageFrame
      title="Recordings"
      description={
        isAdmin
          ? 'Sessions recorded by Record sessions, and recordings users started from the session menu.'
          : 'What you recorded from the session menu.'
      }>
      <AsyncContent data={recordings} errorTitle="Could not load recordings">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            emptyState={
              <EmptyState
                icon={<FilmIcon width={32} height={32} />}
                title="No recordings yet"
                description={
                  isAdmin
                    ? "Turn on Record sessions in a connection's settings to record its sessions, or let users record from the session menu."
                    : 'Start recording from the session menu (Ctrl+Alt+Shift) of a connection that allows it.'
                }
              />
            }
          />
        )}
      </AsyncContent>

      <ConfirmDeleteDialog
        target={deleting}
        title={r => `Delete the recording of ${r.connectionName}?`}
        description="The file is removed from the gateway and cannot be played or downloaded again."
        onDelete={r => api.deleteRecording(r.id)}
        onClose={deleted => {
          if (deleted) {
            showToast({body: 'Deleted the recording.'});
            recordings.reload();
          }
          setDeleting(null);
        }}
      />
    </PageFrame>
  );
}
