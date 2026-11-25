import {useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack} from '@astryxdesign/core/Stack';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {useToast} from '@astryxdesign/core/Toast';
import {ComputerDesktopIcon, PlusIcon} from '@heroicons/react/24/outline';
import {api, type ConnectionSummary, type User} from '../api';
import {href} from '../route';
import {useApiData} from '../useApiData';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import {CLOSED, CREATE, edit, type DrawerState} from '../shell/drawerState';
import ConnectionDrawer from './ConnectionDrawer';
import {nameColumn, protocolColumn, targetColumn} from './columns';

const NoConnectionsIcon = () => <ComputerDesktopIcon width={32} height={32} />;

export default function ConnectionsPage({user}: {user: User}) {
  return user.isAdmin ? <ManageConnectionsPage /> : <MyConnectionsPage />;
}

/** A user's view: the connections granted to them. */
function MyConnectionsPage() {
  const connections = useApiData(api.connections);
  const columns: TableColumn<ConnectionSummary>[] = [
    nameColumn,
    protocolColumn,
    targetColumn,
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(120),
      renderCell: c => <Button label="Connect" size="sm" href={href.session(c.id)} />,
    },
  ];

  return (
    <PageFrame title="Connections">
      <AsyncContent data={connections} errorTitle="Could not load connections">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            emptyState={
              <EmptyState
                icon={<NoConnectionsIcon />}
                title="No connections yet"
                description="Ask an administrator to give you access to a connection."
              />
            }
          />
        )}
      </AsyncContent>
    </PageFrame>
  );
}

/** An administrator's view: every connection, with create, edit and delete. */
function ManageConnectionsPage() {
  const connections = useApiData(api.connections);
  const [drawer, setDrawer] = useState<DrawerState>(CLOSED);
  const [deleting, setDeleting] = useState<ConnectionSummary | null>(null);
  const showToast = useToast();

  const columns: TableColumn<ConnectionSummary>[] = [
    nameColumn,
    protocolColumn,
    targetColumn,
    {
      key: 'guacdHost',
      header: 'Via',
      renderCell: c => (
        <Text color="secondary">
          {/* Without a port the gateway uses its own [guacd] port. */}
          {c.guacdHost ? [c.guacdHost, c.guacdPort].filter(Boolean).join(':') : 'Default guacd'}
        </Text>
      ),
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(160),
      renderCell: c => (
        <HStack gap={1} hAlign="end" vAlign="center">
          <Button label="Connect" size="sm" href={href.session(c.id)} />
          <MoreMenu
            size="sm"
            label={`Options for ${c.name}`}
            items={[
              {label: 'Edit', onClick: () => setDrawer(edit(c.id))},
              {label: 'Delete', onClick: () => setDeleting(c)},
            ]}
          />
        </HStack>
      ),
    },
  ];

  const newConnectionButton = (
    <Button
      label="New connection"
      variant="primary"
      icon={<PlusIcon width={16} height={16} />}
      onClick={() => setDrawer(CREATE)}
    />
  );
  const hasConnections = connections.status === 'ready' && connections.data.length > 0;

  return (
    <PageFrame title="Connections" action={hasConnections ? newConnectionButton : undefined}>
      <AsyncContent data={connections} errorTitle="Could not load connections">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            emptyState={
              <EmptyState
                icon={<NoConnectionsIcon />}
                title="No connections yet"
                description="Add a VNC, RDP, SSH, Telnet or Kubernetes connection to get started."
                actions={newConnectionButton}
              />
            }
          />
        )}
      </AsyncContent>

      <ConnectionDrawer
        state={drawer}
        onClose={() => setDrawer(CLOSED)}
        onSaved={saved => {
          setDrawer(CLOSED);
          showToast({body: `Saved ${saved.name}.`});
          connections.reload();
        }}
      />

      <ConfirmDeleteDialog
        target={deleting}
        title={c => `Delete ${c.name}?`}
        description="Users lose access to this connection. Open sessions keep running until they disconnect."
        onDelete={c => api.deleteConnection(c.id)}
        onClose={deleted => {
          if (deleted && deleting) {
            showToast({body: `Deleted ${deleting.name}.`});
            connections.reload();
          }
          setDeleting(null);
        }}
      />
    </PageFrame>
  );
}
