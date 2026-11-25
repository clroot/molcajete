import {useCallback, useEffect, useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Badge} from '@astryxdesign/core/Badge';
import {Banner} from '@astryxdesign/core/Banner';
import {Link} from '@astryxdesign/core/Link';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {StatusDot} from '@astryxdesign/core/StatusDot';
import {Timestamp} from '@astryxdesign/core/Timestamp';
import {Spinner} from '@astryxdesign/core/Spinner';
import {Center} from '@astryxdesign/core/Center';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {useToast} from '@astryxdesign/core/Toast';
import {PlusIcon} from '@heroicons/react/24/outline';
import {api, errorMessage, type User} from '../api';
import PageFrame from './PageFrame';
import UserDrawer from './UserDrawer';

export default function UsersPage({currentUser}: {currentUser: User}) {
  const [users, setUsers] = useState<User[] | null>(null);
  const [loadError, setLoadError] = useState('');
  // undefined: drawer closed; null: new user; string: editing that id.
  const [editing, setEditing] = useState<string | null | undefined>(undefined);
  const [deleting, setDeleting] = useState<User | null>(null);
  const [isDeleting, setIsDeleting] = useState(false);
  const showToast = useToast();

  const load = useCallback(async () => {
    try {
      setUsers(await api.users());
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
      await api.deleteUser(deleting.id);
      showToast({body: `Deleted ${deleting.username}.`});
      setDeleting(null);
      load();
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    } finally {
      setIsDeleting(false);
    }
  };

  const columns: TableColumn<User>[] = [
    {
      key: 'username',
      header: 'Username',
      renderCell: u => (
        <HStack gap={2} vAlign="center">
          <Link
            href="#/users"
            weight="medium"
            onClick={event => {
              event.preventDefault();
              setEditing(u.id);
            }}>
            {u.username}
          </Link>
          {u.id === currentUser.id && <Text color="secondary">(you)</Text>}
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
      renderCell: u => (
        <HStack gap={2} vAlign="center">
          <StatusDot variant={u.disabled ? 'neutral' : 'success'} label={u.disabled ? 'Disabled' : 'Active'} />
          <Text>{u.disabled ? 'Disabled' : 'Active'}</Text>
        </HStack>
      ),
    },
    {
      key: 'lastLoginAt',
      header: 'Last sign-in',
      width: pixel(180),
      renderCell: u =>
        u.lastLoginAt ? <Timestamp value={u.lastLoginAt} format="auto" /> : <Text color="secondary">Never</Text>,
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
              {label: 'Edit', onClick: () => setEditing(u.id)},
              ...(u.id === currentUser.id ? [] : [{label: 'Delete', onClick: () => setDeleting(u)}]),
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
          onClick={() => setEditing(null)}
        />
      }>
      {loadError ? (
        <Banner status="error" title="Could not load users" description={loadError} />
      ) : users === null ? (
        <Center axis="both" padding={10}>
          <Spinner size="lg" />
        </Center>
      ) : (
        <VStack gap={4}>
          <Table data={users} columns={columns} idKey="id" hasHover />
        </VStack>
      )}

      <UserDrawer
        userId={editing}
        currentUserId={currentUser.id}
        onClose={() => setEditing(undefined)}
        onSaved={saved => {
          setEditing(undefined);
          showToast({body: `Saved ${saved.username}.`});
          load();
        }}
      />

      <AlertDialog
        isOpen={deleting !== null}
        onOpenChange={open => !open && setDeleting(null)}
        title={`Delete ${deleting?.username ?? 'user'}?`}
        description="They are signed out and can no longer open any connection."
        actionLabel="Delete"
        isActionLoading={isDeleting}
        onAction={remove}
      />
    </PageFrame>
  );
}
