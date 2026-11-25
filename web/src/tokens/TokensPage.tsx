import {useCallback, useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Button} from '@astryxdesign/core/Button';
import {Code} from '@astryxdesign/core/Code';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {SegmentedControl, SegmentedControlItem} from '@astryxdesign/core/SegmentedControl';
import {useToast} from '@astryxdesign/core/Toast';
import {CommandLineIcon} from '@heroicons/react/24/outline';
import {api, type ApiToken, type CreatedApiToken, type User} from '../api';
import {useApiData} from '../useApiData';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import CreateTokenDialog from './CreateTokenDialog';
import TokenCreatedDialog from './TokenCreatedDialog';
import {isExpired} from './format';

type Scope = 'mine' | 'all';

/** The signed-in user's API tokens; administrators can list everyone's. */
export default function TokensPage({user}: {user: User}) {
  const [scope, setScope] = useState<Scope>('mine');
  const tokens = useApiData(useCallback(() => api.apiTokens(scope), [scope]));
  const [isCreating, setIsCreating] = useState(false);
  const [created, setCreated] = useState<CreatedApiToken | null>(null);
  const [revoking, setRevoking] = useState<ApiToken | null>(null);
  const showToast = useToast();

  const columns: TableColumn<ApiToken>[] = [
    {
      key: 'name',
      header: 'Token',
      renderCell: t => (
        <VStack gap={0.5} hAlign="start">
          <Text weight="medium">{t.name}</Text>
          <Code color="secondary">{`${t.prefix}…`}</Code>
        </VStack>
      ),
    },
    ...(scope === 'all'
      ? [
          {
            key: 'username',
            header: 'Owner',
            width: pixel(160),
            renderCell: (t: ApiToken) => <Text>{t.username}</Text>,
          },
        ]
      : []),
    {
      key: 'allowedIps',
      header: 'Allowed from',
      width: pixel(200),
      renderCell: t =>
        t.allowedIps.length === 0 ? (
          <Text color="secondary">Any address</Text>
        ) : (
          <Text>{t.allowedIps.join(', ')}</Text>
        ),
    },
    {
      key: 'lastUsedAt',
      header: 'Last used',
      width: pixel(200),
      renderCell: t =>
        t.lastUsedAt === null ? (
          <Text color="secondary">Never</Text>
        ) : (
          <VStack gap={0.5}>
            <Timestamp value={t.lastUsedAt} format="auto" color="primary" type="body" />
            {t.lastUsedFrom && <Text color="secondary">{t.lastUsedFrom}</Text>}
          </VStack>
        ),
    },
    {
      key: 'expiresAt',
      header: 'Expires',
      width: pixel(180),
      renderCell: t =>
        isExpired(t) ? (
          <HStack gap={2} vAlign="center">
            <StatusDot variant="error" label="Expired" />
            <Text>Expired</Text>
          </HStack>
        ) : (
          <Timestamp value={t.expiresAt} format="date" color="primary" type="body" />
        ),
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(80),
      renderCell: t => (
        <HStack hAlign="end">
          <MoreMenu
            size="sm"
            label={`Options for ${t.name}`}
            items={[{label: 'Revoke', onClick: () => setRevoking(t)}]}
          />
        </HStack>
      ),
    },
  ];

  return (
    <PageFrame
      title="API tokens"
      description="For scripts and other applications: each acts as its owner, with the owner's role and access."
      action={
        <HStack gap={3} vAlign="center" wrap="wrap">
          {user.isAdmin && (
            <SegmentedControl label="Whose tokens" value={scope} onChange={value => setScope(value as Scope)}>
              <SegmentedControlItem value="mine" label="Mine" />
              <SegmentedControlItem value="all" label="Everyone's" />
            </SegmentedControl>
          )}
          <Button label="New token" variant="primary" onClick={() => setIsCreating(true)} />
        </HStack>
      }>
      <AsyncContent data={tokens} errorTitle="Could not load API tokens">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            emptyState={
              <EmptyState
                icon={<CommandLineIcon width={32} height={32} />}
                title="No API tokens"
                description="Create one for a script or an application that uses molcajete's API."
              />
            }
          />
        )}
      </AsyncContent>

      {isCreating && (
        <CreateTokenDialog
          onClose={() => setIsCreating(false)}
          onCreated={token => {
            setIsCreating(false);
            setCreated(token);
            tokens.reload();
          }}
        />
      )}
      <TokenCreatedDialog created={created} onClose={() => setCreated(null)} />

      <ConfirmDeleteDialog
        target={revoking}
        title={t => `Revoke ${t.name}?`}
        description="Anything still using it is refused from now on, and the session links it made stop working."
        actionLabel="Revoke"
        onDelete={t => api.revokeApiToken(t.id)}
        onClose={revoked => {
          if (revoked) {
            showToast({body: 'Revoked the token.'});
            tokens.reload();
          }
          setRevoking(null);
        }}
      />
    </PageFrame>
  );
}
