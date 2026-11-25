import {useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {useToast} from '@astryxdesign/core/Toast';
import {PlusIcon} from '@heroicons/react/24/outline';
import {api, type User} from '../api';
import {useApiData} from '../useApiData';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import {CLOSED, CREATE, edit, type DrawerState} from '../shell/drawerState';
import UserDrawer from './UserDrawer';

export default function UsersPage({currentUser}: {currentUser: User}) {
  const users = useApiData(api.users);
  const [drawer, setDrawer] = useState<DrawerState>(CLOSED);
  const [deleting, setDeleting] = useState<User | null>(null);
  const showToast = useToast();
  const isCurrentUser = (u: User) => u.id === currentUser.id;

  const columns: TableColumn<User>[] = [
    {
      key: 'username',
      header: 'Username',
      renderCell: u => (
        <HStack gap={2} vAlign="center">
          <Text weight="medium">{u.username}</Text>
          {isCurrentUser(u) && <Text color="secondary">(you)</Text>}
        </HStack>
      ),
    },
    {
      key: 'isAdmin',
      header: 'Role',
      width: pixel(140),
      renderCell: u => (u.isAdmin ? <Badge variant="info" label="Administrator" /> : <Text>User</Text>),
    },
    {
      key: 'disabled',
      header: 'Status',
      width: pixel(140),
      renderCell: u => {
        const status = u.disabled ? 'Disabled' : 'Active';
        return (
          <HStack gap={2} vAlign="center">
            <StatusDot variant={u.disabled ? 'neutral' : 'success'} label={status} />
            <Text>{status}</Text>
          </HStack>
        );
      },
    },
    {
      key: 'lastLoginAt',
      header: 'Last sign-in',
      width: pixel(180),
      renderCell: u =>
        u.lastLoginAt ? (
          <Timestamp value={u.lastLoginAt} format="auto" />
        ) : (
          <Text color="secondary">Never</Text>
        ),
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(80),
      renderCell: u => (
        <HStack hAlign="end">
          <MoreMenu
            size="sm"
            label={`Options for ${u.username}`}
            items={[
              {label: 'Edit', onClick: () => setDrawer(edit(u.id))},
              // The server refuses to delete your own account.
              ...(isCurrentUser(u) ? [] : [{label: 'Delete', onClick: () => setDeleting(u)}]),
            ]}
          />
        </HStack>
      ),
    },
  ];

  return (
    <PageFrame
      title="Users"
      action={
        <Button
          label="New user"
          variant="primary"
          icon={<PlusIcon width={16} height={16} />}
          onClick={() => setDrawer(CREATE)}
        />
      }>
      <AsyncContent data={users} errorTitle="Could not load users">
        {list => <Table data={list} columns={columns} idKey="id" hasHover />}
      </AsyncContent>

      <UserDrawer
        state={drawer}
        currentUserId={currentUser.id}
        onClose={() => setDrawer(CLOSED)}
        onSaved={saved => {
          setDrawer(CLOSED);
          showToast({body: `Saved ${saved.username}.`});
          users.reload();
        }}
      />

      <ConfirmDeleteDialog
        target={deleting}
        title={u => `Delete ${u.username}?`}
        description="They are signed out and can no longer open any connection."
        onDelete={u => api.deleteUser(u.id)}
        onClose={deleted => {
          if (deleted && deleting) {
            showToast({body: `Deleted ${deleting.username}.`});
            users.reload();
          }
          setDeleting(null);
        }}
      />
    </PageFrame>
  );
}
