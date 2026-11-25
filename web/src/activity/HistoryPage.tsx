import {useCallback, useMemo, useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {TextInput} from '@astryxdesign/core/TextInput';
import {DateRangeInput, type DateRange} from '@astryxdesign/core/DateRangeInput';
import {Pagination} from '@astryxdesign/core/Pagination';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {ArrowDownTrayIcon, ClockIcon} from '@heroicons/react/24/outline';
import {useApiData} from '../useApiData';
import {useSettled} from '../useSettled';
import {href} from '../route';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import {protocolLabel} from '../connections/protocols';
import {formatClock, formatSize} from '../recordings/format';
import {localDayStart} from '../audit/format';
import {historyCsvUrl, sessionsApi, type HistoryEntry, type HistoryFilter} from './api';
import {outcome} from './format';

const PAGE_SIZE = 50;

/** Closed sessions, most recent first, for administrators. */
export default function HistoryPage() {
  const [user, setUser] = useState('');
  const [connection, setConnection] = useState('');
  const [dates, setDates] = useState<DateRange | null>(null);

  const settledUser = useSettled(user.trim());
  const settledConnection = useSettled(connection.trim());
  const filter: HistoryFilter = useMemo(
    () => ({
      user: settledUser,
      connection: settledConnection,
      from: dates ? localDayStart(dates.start) : undefined,
      to: dates ? localDayStart(dates.end, 1) : undefined,
    }),
    [settledUser, settledConnection, dates],
  );
  // A new filter starts again from the most recent sessions.
  const [paging, setPaging] = useState({filter, page: 1});
  const page = paging.filter === filter ? paging.page : 1;
  const setPage = (next: number) => setPaging({filter, page: next});

  const fetchPage = useCallback(
    () => sessionsApi.history(filter, PAGE_SIZE, (page - 1) * PAGE_SIZE),
    [filter, page],
  );
  const entries = useApiData(fetchPage);

  const columns: TableColumn<HistoryEntry>[] = [
    {
      key: 'startedAt',
      header: 'Started',
      width: pixel(190),
      renderCell: e => <Timestamp value={e.startedAt} format="date_time" color="primary" type="body" />,
    },
    {
      key: 'username',
      header: 'User',
      renderCell: e => (
        <VStack gap={0.5}>
          <Text weight="medium">{e.username}</Text>
          {e.peerAddress && <Text color="secondary">{e.peerAddress}</Text>}
        </VStack>
      ),
    },
    {
      key: 'connectionName',
      header: 'Connection',
      renderCell: e => (
        <VStack gap={0.5}>
          <Text>{e.connectionName}</Text>
          <Text color="secondary">{protocolLabel(e.protocol)}</Text>
        </VStack>
      ),
    },
    {
      key: 'endedAt',
      header: 'Duration',
      align: 'end',
      width: pixel(100),
      renderCell: e => <Text>{formatClock(e.endedAt - e.startedAt)}</Text>,
    },
    {
      key: 'endStatus',
      header: 'Ended',
      renderCell: e => {
        const {label, variant} = outcome(e);
        return (
          <VStack gap={0.5}>
            <HStack gap={2} vAlign="center">
              <StatusDot variant={variant} label={label} />
              <Text>{label}</Text>
            </HStack>
            {e.endedBy && <Text color="secondary">by {e.endedBy}</Text>}
          </VStack>
        );
      },
    },
    {
      key: 'bytesToBrowser',
      header: 'Traffic',
      width: pixel(150),
      renderCell: e => (
        <VStack gap={0.5}>
          <Text>{formatSize(e.bytesToBrowser)} received</Text>
          <Text color="secondary">{formatSize(e.bytesToGuacd)} sent</Text>
        </VStack>
      ),
    },
    {
      key: 'recordingId',
      header: 'Recording',
      width: pixel(110),
      renderCell: e =>
        e.recordingId ? (
          <Link href={href.playback(e.recordingId)}>Play</Link>
        ) : (
          <Text color="secondary">—</Text>
        ),
    },
  ];

  return (
    <PageFrame
      title="Connection history"
      description="Sessions that have closed: who connected to what, for how long, and how it ended."
      action={
        <Button
          label="Export CSV"
          icon={<ArrowDownTrayIcon width={16} height={16} />}
          onClick={() => location.assign(historyCsvUrl(filter))}
        />
      }>
      <VStack gap={4}>
        <HStack gap={3} wrap="wrap" vAlign="end">
          <TextInput
            label="User"
            placeholder="Username"
            value={user}
            onChange={setUser}
            hasClear
            width={200}
          />
          <TextInput
            label="Connection"
            placeholder="Name or id"
            value={connection}
            onChange={setConnection}
            hasClear
            width={200}
          />
          <DateRangeInput label="Started" value={dates} onChange={setDates} placeholder="Any time" />
        </HStack>

        <AsyncContent data={entries} errorTitle="Could not load the connection history">
          {result => (
            <VStack gap={3}>
              <Table
                data={result.entries}
                columns={columns}
                idKey="id"
                hasHover
                verticalAlign="top"
                emptyState={
                  <EmptyState
                    icon={<ClockIcon width={32} height={32} />}
                    title="No sessions"
                    description="Nothing matches these filters."
                  />
                }
              />
              {(page > 1 || result.hasMore) && (
                <HStack hAlign="end">
                  <Pagination
                    page={page}
                    onChange={setPage}
                    hasMore={result.hasMore}
                    pageSize={PAGE_SIZE}
                    variant="compact"
                    label="Connection history pages"
                  />
                </HStack>
              )}
            </VStack>
          )}
        </AsyncContent>
      </VStack>
    </PageFrame>
  );
}
