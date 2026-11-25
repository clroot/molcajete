import {useCallback, useMemo, useState} from 'react';
import {Table, pixel, useTableRowExpansion, type TableColumn} from '@astryxdesign/core/Table';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Code} from '@astryxdesign/core/Code';
import {Text} from '@astryxdesign/core/Text';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Selector} from '@astryxdesign/core/Selector';
import {DateRangeInput, type DateRange} from '@astryxdesign/core/DateRangeInput';
import {Pagination} from '@astryxdesign/core/Pagination';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {ArrowDownTrayIcon, ClipboardDocumentListIcon} from '@heroicons/react/24/outline';
import {api, auditCsvUrl, type AuditEntry, type AuditFilter} from '../api';
import {useApiData} from '../useApiData';
import {useSettled} from '../useSettled';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import {actionGroups, changeRows, localDayStart, type ChangeRow} from './format';

const PAGE_SIZE = 50;
/** Who changed what, signed in or opened a recording; for administrators. */
export default function AuditPage() {
  const [actor, setActor] = useState('');
  const [target, setTarget] = useState('');
  const [action, setAction] = useState<string | null>(null);
  const [dates, setDates] = useState<DateRange | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());

  const settledActor = useSettled(actor.trim());
  const settledTarget = useSettled(target.trim());
  const filter: AuditFilter = useMemo(
    () => ({
      actor: settledActor,
      target: settledTarget,
      action: action ?? undefined,
      from: dates ? localDayStart(dates.start) : undefined,
      to: dates ? localDayStart(dates.end, 1) : undefined,
    }),
    [settledActor, settledTarget, action, dates],
  );
  // A new filter starts again from the newest entries.
  const [paging, setPaging] = useState({filter, page: 1});
  const page = paging.filter === filter ? paging.page : 1;
  const setPage = (next: number) => setPaging({filter, page: next});

  const fetchPage = useCallback(
    () => api.auditLog(filter, PAGE_SIZE, (page - 1) * PAGE_SIZE),
    [filter, page],
  );
  const entries = useApiData(fetchPage);
  const actions = useApiData(api.auditActions);

  const actionOptions = useMemo(
    () =>
      actions.status === 'ready'
        ? actionGroups(actions.data).map(group => ({
            type: 'section' as const,
            title: group.prefix,
            options: [
              ...(group.actions.length > 1
                ? [{value: group.prefix, label: `Any ${group.prefix} action`}]
                : []),
              ...group.actions.map(name => ({value: name, label: name})),
            ],
          }))
        : [],
    [actions],
  );

  const expansion = useTableRowExpansion<AuditEntry>({
    expandedKeys: expanded,
    onToggle: key =>
      setExpanded(current => {
        const next = new Set(current);
        if (!next.delete(key)) next.add(key);
        return next;
      }),
    getRowKey: entry => entry.id,
    getRowLabel: entry => entry.action,
    getIsItemExpandable: entry => entry.changes !== null,
    renderExpanded: entry => <Changes rows={changeRows(entry.changes)} />,
  });

  const columns: TableColumn<AuditEntry>[] = [
    {
      key: 'occurredAt',
      header: 'Time',
      width: pixel(190),
      renderCell: e => <Timestamp value={e.occurredAt} format="date_time" color="primary" type="body" />,
    },
    {
      key: 'action',
      header: 'Action',
      width: pixel(190),
      renderCell: e => <Code>{e.action}</Code>,
    },
    {
      key: 'actorName',
      header: 'By',
      renderCell: e => (
        <VStack gap={0.5}>
          <Text color={e.actorName ? 'primary' : 'secondary'}>{e.actorName ?? 'Gateway'}</Text>
          {e.sourceAddress && <Text color="secondary">{e.sourceAddress}</Text>}
        </VStack>
      ),
    },
    {
      key: 'targetName',
      header: 'Target',
      renderCell: e =>
        e.targetType ? (
          <VStack gap={0.5}>
            <Text>{e.targetName ?? e.targetId}</Text>
            <Text color="secondary">{e.targetType}</Text>
          </VStack>
        ) : null,
    },
  ];

  return (
    <PageFrame
      title="Audit log"
      description="Changes to users, groups, connections and folders, sign-ins, recordings opened, and API tokens and session links."
      action={
        <Button
          label="Export CSV"
          icon={<ArrowDownTrayIcon width={16} height={16} />}
          onClick={() => location.assign(auditCsvUrl(filter))}
        />
      }>
      <VStack gap={4}>
        <HStack gap={3} wrap="wrap" vAlign="end">
          <TextInput
            label="By"
            placeholder="Username"
            value={actor}
            onChange={setActor}
            hasClear
            width={200}
          />
          <Selector
            label="Action"
            placeholder="All actions"
            options={actionOptions}
            value={action}
            onChange={setAction}
            hasClear
            hasSearch
            isLoading={actions.status === 'loading'}
            width={240}
          />
          <TextInput
            label="Target"
            placeholder="Name or id"
            value={target}
            onChange={setTarget}
            hasClear
            width={200}
          />
          <DateRangeInput label="Dates" value={dates} onChange={setDates} placeholder="Any time" />
        </HStack>

        <AsyncContent data={entries} errorTitle="Could not load the audit log">
          {result => (
            <VStack gap={3}>
              <Table
                data={result.entries}
                columns={columns}
                idKey="id"
                hasHover
                verticalAlign="top"
                plugins={{expansion}}
                emptyState={
                  <EmptyState
                    icon={<ClipboardDocumentListIcon width={32} height={32} />}
                    title="No entries"
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
                    label="Audit log pages"
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

/** What changed in one entry, field by field. */
function Changes({rows}: {rows: ChangeRow[]}) {
  const columns: TableColumn<ChangeRow>[] = [
    {key: 'field', header: 'Field', width: pixel(220), renderCell: r => <Code>{r.field}</Code>},
    {key: 'before', header: 'Before', renderCell: r => <Text>{r.before}</Text>},
    {key: 'after', header: 'After', renderCell: r => <Text>{r.after}</Text>},
  ];
  return <Table data={rows} columns={columns} idKey="field" density="compact" />;
}
