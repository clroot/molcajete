import {useCallback, useEffect, useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Banner} from '@astryxdesign/core/Banner';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {Spinner} from '@astryxdesign/core/Spinner';
import {Center} from '@astryxdesign/core/Center';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {useToast} from '@astryxdesign/core/Toast';
import {ComputerDesktopIcon, PlusIcon} from '@heroicons/react/24/outline';
import {api, errorMessage, type ConnectionSummary, type User} from '../api';
import {href} from '../route';
import {protocolLabel} from '../protocols';
import PageFrame from './PageFrame';
import ConnectionDrawer from './ConnectionDrawer';

function target(c: ConnectionSummary) {
  if (!c.hostname) return '';
  return c.port ? `${c.hostname}:${c.port}` : c.hostname;
}

export default function ConnectionsPage({user}: {user: User}) {
  const [connections, setConnections] = useState<ConnectionSummary[] | null>(null);
  const [loadError, setLoadError] = useState('');
  // undefined: drawer closed; null: new connection; string: editing that id.
  const [editing, setEditing] = useState<string | null | undefined>(undefined);
  const [deleting, setDeleting] = useState<ConnectionSummary | null>(null);
  const [isDeleting, setIsDeleting] = useState(false);
  const showToast = useToast();

  const load = useCallback(async () => {
    try {
      setConnections(await api.connections());
      setLoadError('');
    } catch (err) {
      setLoadError(errorMessage(err));
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const remove = async () => {
    if (!deleting) return;
    setIsDeleting(true);
    try {
      await api.deleteConnection(deleting.id);
      showToast({body: `Deleted ${deleting.name}.`});
      setDeleting(null);
      load();
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    } finally {
      setIsDeleting(false);
    }
  };

  const columns: TableColumn<ConnectionSummary>[] = [
    {
      key: 'name',
      header: 'Name',
      renderCell: c => (
        <VStack gap={0.5}>
          <Link href={href.session(c.id)} weight="medium">
            {c.name}
          </Link>
          <Text type="supporting" color="secondary">
            {c.id}
          </Text>
        </VStack>
      ),
    },
    {key: 'protocol', header: 'Protocol', width: pixel(120), renderCell: c => <Badge label={protocolLabel(c.protocol)} />},
    {
      key: 'hostname',
      header: 'Target',
      renderCell: c => <Text color={target(c) ? 'primary' : 'secondary'}>{target(c) || 'Not set'}</Text>,
    },
    ...(user.isAdmin
      ? [
          {
            key: 'guacdHost',
            header: 'Via',
            renderCell: (c: ConnectionSummary) => (
              <Text color="secondary">
                {c.guacdHost ? `${c.guacdHost}:${c.guacdPort ?? 4822}` : 'Default guacd'}
              </Text>
            ),
          },
        ]
      : []),
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(user.isAdmin ? 160 : 120),
      renderCell: c => (
        <HStack gap={1} hAlign="end" vAlign="center">
          <Button label="Connect" size="sm" href={href.session(c.id)} />
          {user.isAdmin && (
            <MoreMenu
              size="sm"
              label={`Options for ${c.name}`}
              items={[
                {label: 'Edit', onClick: () => setEditing(c.id)},
                {label: 'Delete', onClick: () => setDeleting(c)},
              ]}
            />
          )}
        </HStack>
      ),
    },
  ];

  const emptyState = (
    <EmptyState
      icon={<ComputerDesktopIcon width={32} height={32} />}
      title="No connections yet"
      description={
        user.isAdmin
          ? 'Add a VNC, RDP, SSH, Telnet or Kubernetes connection to get started.'
          : 'Ask an administrator to give you access to a connection.'
      }
      actions={
        user.isAdmin ? (
          <Button label="New connection" variant="primary" onClick={() => setEditing(null)} />
        ) : undefined
      }
    />
  );

  return (
    <PageFrame
      title="Connections"
      action={
        user.isAdmin && connections?.length ? (
          <Button
            label="New connection"
            variant="primary"
            icon={<PlusIcon width={16} height={16} />}
            onClick={() => setEditing(null)}
          />
        ) : undefined
      }>
      {loadError ? (
        <Banner status="error" title="Could not load connections" description={loadError} />
      ) : connections === null ? (
        <Center axis="both" padding={10}>
          <Spinner size="lg" />
        </Center>
      ) : (
        <Table data={connections} columns={columns} idKey="id" hasHover emptyState={emptyState} />
      )}

      {user.isAdmin && (
        <ConnectionDrawer
          connectionId={editing}
          onClose={() => setEditing(undefined)}
          onSaved={saved => {
            setEditing(undefined);
            showToast({body: `Saved ${saved.name}.`});
            load();
          }}
        />
      )}

      <AlertDialog
        isOpen={deleting !== null}
        onOpenChange={open => !open && setDeleting(null)}
        title={`Delete ${deleting?.name ?? 'connection'}?`}
        description="Users lose access to this connection. Open sessions keep running until they disconnect."
        actionLabel="Delete"
        isActionLoading={isDeleting}
        onAction={remove}
      />
    </PageFrame>
  );
}
