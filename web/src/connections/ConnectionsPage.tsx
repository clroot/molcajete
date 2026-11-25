import {useState} from 'react';
import {pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack} from '@astryxdesign/core/Stack';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {useToast} from '@astryxdesign/core/Toast';
import {ComputerDesktopIcon, FolderPlusIcon, PlusIcon} from '@heroicons/react/24/outline';
import {api, canManageConnections, type ConnectionSummary, type Folder, type User} from '../api';
import {href} from '../route';
import {useApiData} from '../useApiData';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import {CLOSED, CREATE, edit, type DrawerState} from '../shell/drawerState';
import ConnectionDrawer from './ConnectionDrawer';
import ConnectionTree from './ConnectionTree';
import FolderDialog, {type FolderDialogState} from './FolderDialog';
import MoveDialog, {type MoveTarget} from './MoveDialog';
import AccessDialog, {type AccessTarget} from './AccessDialog';
import {nameColumn, protocolColumn, rowLabel, targetColumn, viaColumn} from './columns';
import {folderPath, type TreeRow} from './tree';

const NoConnectionsIcon = () => <ComputerDesktopIcon width={32} height={32} />;

type Listing = {folders: Folder[]; connections: ConnectionSummary[]};

async function loadListing(): Promise<Listing> {
  const [folders, connections] = await Promise.all([api.folders(), api.connections()]);
  return {folders, connections};
}

export default function ConnectionsPage({user}: {user: User}) {
  return canManageConnections(user) ? <ManageConnectionsPage /> : <MyConnectionsPage />;
}

const connectButton = (c: ConnectionSummary) => (
  <Button label="Connect" size="sm" href={href.session(c.id)} />
);

/** A user's view: the connections granted to them, in their folders. */
function MyConnectionsPage() {
  const listing = useApiData(loadListing);
  const columns: TableColumn<TreeRow>[] = [
    nameColumn,
    protocolColumn,
    targetColumn,
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(120),
      renderCell: row => (row.kind === 'connection' ? connectButton(row.connection) : null),
    },
  ];

  return (
    <PageFrame title="Connections">
      <AsyncContent data={listing} errorTitle="Could not load connections">
        {({folders, connections}) => (
          <ConnectionTree
            folders={folders}
            connections={connections}
            columns={columns}
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

/**
 * A connection manager's or administrator's view: every folder and
 * connection, with create, edit, move, access and delete.
 */
function ManageConnectionsPage() {
  const listing = useApiData(loadListing);
  const folders = listing.status === 'ready' ? listing.data.folders : [];
  const [drawer, setDrawer] = useState<DrawerState>(CLOSED);
  const [newConnectionFolder, setNewConnectionFolder] = useState<string | null>(null);
  const [folderDialog, setFolderDialog] = useState<FolderDialogState | null>(null);
  const [moving, setMoving] = useState<MoveTarget | null>(null);
  const [granting, setGranting] = useState<AccessTarget | null>(null);
  const [deleting, setDeleting] = useState<TreeRow | null>(null);
  const showToast = useToast();

  const createConnectionIn = (folderId: string | null) => {
    setNewConnectionFolder(folderId);
    setDrawer(CREATE);
  };

  const rowMenu = (row: TreeRow) => {
    if (row.kind === 'connection') {
      const c = row.connection;
      return [
        {label: 'Edit', onClick: () => setDrawer(edit(c.id))},
        {
          label: 'Move to…',
          onClick: () => setMoving({kind: 'connection', id: c.id, name: c.name, parentId: c.parentId}),
        },
        {label: 'Access…', onClick: () => setGranting({kind: 'connection', id: c.id, name: c.name})},
        {label: 'Delete', onClick: () => setDeleting(row)},
      ];
    }
    const f = row.folder;
    return [
      {label: 'New connection here', onClick: () => createConnectionIn(f.id)},
      {
        label: 'New folder here',
        onClick: () =>
          setFolderDialog({mode: 'create', parentId: f.id, parentName: folderPath(folders, f.id)}),
      },
      {label: 'Rename', onClick: () => setFolderDialog({mode: 'rename', folder: f})},
      {
        label: 'Move to…',
        onClick: () => setMoving({kind: 'folder', id: f.id, name: f.name, parentId: f.parentId}),
      },
      {label: 'Access…', onClick: () => setGranting({kind: 'folder', id: f.id, name: f.name})},
      {label: 'Delete', onClick: () => setDeleting(row)},
    ];
  };

  const columns: TableColumn<TreeRow>[] = [
    nameColumn,
    protocolColumn,
    targetColumn,
    viaColumn,
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(160),
      renderCell: row => (
        <HStack gap={1} hAlign="end" vAlign="center">
          {row.kind === 'connection' && connectButton(row.connection)}
          <MoreMenu size="sm" label={`Options for ${rowLabel(row)}`} items={rowMenu(row)} />
        </HStack>
      ),
    },
  ];

  const newConnectionButton = (
    <Button
      label="New connection"
      variant="primary"
      icon={<PlusIcon width={16} height={16} />}
      onClick={() => createConnectionIn(null)}
    />
  );
  const actions = (
    <HStack gap={2} wrap="wrap">
      <Button
        label="New folder"
        variant="secondary"
        icon={<FolderPlusIcon width={16} height={16} />}
        onClick={() => setFolderDialog({mode: 'create', parentId: null})}
      />
      {newConnectionButton}
    </HStack>
  );
  const isEmpty = listing.status === 'ready' && listing.data.connections.length === 0 && folders.length === 0;

  return (
    <PageFrame title="Connections" action={listing.status === 'ready' && !isEmpty ? actions : undefined}>
      <AsyncContent data={listing} errorTitle="Could not load connections">
        {({connections}) => (
          <ConnectionTree
            folders={folders}
            connections={connections}
            columns={columns}
            emptyState={
              <EmptyState
                icon={<NoConnectionsIcon />}
                title="No connections yet"
                description="Add a VNC, RDP, SSH, Telnet or Kubernetes connection to get started."
                actions={actions}
              />
            }
          />
        )}
      </AsyncContent>

      <ConnectionDrawer
        state={drawer}
        folders={folders}
        newConnectionFolder={newConnectionFolder}
        onClose={() => setDrawer(CLOSED)}
        onSaved={saved => {
          setDrawer(CLOSED);
          showToast({body: `Saved ${saved.name}.`});
          listing.reload();
        }}
      />

      {folderDialog && (
        <FolderDialog
          key={folderDialog.mode === 'rename' ? folderDialog.folder.id : `new:${folderDialog.parentId}`}
          state={folderDialog}
          onClose={() => setFolderDialog(null)}
          onSaved={folder => {
            setFolderDialog(null);
            showToast({body: `Saved ${folder.name}.`});
            listing.reload();
          }}
        />
      )}

      {moving && (
        <MoveDialog
          key={`${moving.kind}:${moving.id}`}
          target={moving}
          folders={folders}
          onClose={() => setMoving(null)}
          onMoved={(target, folderName) => {
            setMoving(null);
            showToast({body: `Moved ${target.name} to ${folderName}.`});
            listing.reload();
          }}
        />
      )}

      {granting && (
        <AccessDialog
          key={`${granting.kind}:${granting.id}`}
          target={granting}
          onClose={() => setGranting(null)}
          onSaved={target => {
            setGranting(null);
            showToast({body: `Saved access to ${target.name}.`});
          }}
        />
      )}

      <ConfirmDeleteDialog
        target={deleting}
        title={row => `Delete ${rowLabel(row)}?`}
        description={
          deleting?.kind === 'folder'
            ? 'What it holds moves up to the folder above. Grants on this folder are removed, so users may lose access to the connections in it.'
            : 'Users lose access to this connection. Open sessions keep running until they disconnect.'
        }
        onDelete={row =>
          row.kind === 'folder' ? api.deleteFolder(row.folder.id) : api.deleteConnection(row.connection.id)
        }
        onClose={deleted => {
          if (deleted && deleting) {
            showToast({body: `Deleted ${rowLabel(deleting)}.`});
            listing.reload();
          }
          setDeleting(null);
        }}
      />
    </PageFrame>
  );
}
