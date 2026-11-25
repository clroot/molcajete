import {useState} from 'react';
import {Table, pixel, type TableColumn} from '@astryxdesign/core/Table';
import {Button} from '@astryxdesign/core/Button';
import {HStack} from '@astryxdesign/core/Stack';
import {Text} from '@astryxdesign/core/Text';
import {MoreMenu} from '@astryxdesign/core/MoreMenu';
import {EmptyState} from '@astryxdesign/core/EmptyState';
import {useToast} from '@astryxdesign/core/Toast';
import {PlusIcon, UserGroupIcon} from '@heroicons/react/24/outline';
import {api, type Group} from '../api';
import {useApiData} from '../useApiData';
import PageFrame from '../shell/PageFrame';
import AsyncContent from '../shell/AsyncContent';
import ConfirmDeleteDialog from '../shell/ConfirmDeleteDialog';
import {CLOSED, CREATE, edit, type DrawerState} from '../shell/drawerState';
import RoleBadge from '../access/RoleBadge';
import GroupDrawer from './GroupDrawer';

const NoGroupsIcon = () => <UserGroupIcon width={32} height={32} />;

export default function GroupsPage() {
  const groups = useApiData(api.groups);
  const [drawer, setDrawer] = useState<DrawerState>(CLOSED);
  const [deleting, setDeleting] = useState<Group | null>(null);
  const showToast = useToast();

  const columns: TableColumn<Group>[] = [
    {key: 'name', header: 'Name', renderCell: g => <Text weight="medium">{g.name}</Text>},
    {key: 'role', header: 'Role', width: pixel(180), renderCell: g => <RoleBadge role={g.role} />},
    {
      key: 'memberCount',
      header: 'Members',
      width: pixel(120),
      renderCell: g => <Text>{g.memberCount}</Text>,
    },
    {
      key: 'actions',
      header: 'Actions',
      align: 'end',
      width: pixel(80),
      renderCell: g => (
        <HStack hAlign="end">
          <MoreMenu
            size="sm"
            label={`Options for ${g.name}`}
            items={[
              {label: 'Edit', onClick: () => setDrawer(edit(g.id))},
              {label: 'Delete', onClick: () => setDeleting(g)},
            ]}
          />
        </HStack>
      ),
    },
  ];

  const newGroupButton = (
    <Button
      label="New group"
      variant="primary"
      icon={<PlusIcon width={16} height={16} />}
      onClick={() => setDrawer(CREATE)}
    />
  );
  const hasGroups = groups.status === 'ready' && groups.data.length > 0;

  return (
    <PageFrame title="Groups" action={hasGroups ? newGroupButton : undefined}>
      <AsyncContent data={groups} errorTitle="Could not load groups">
        {list => (
          <Table
            data={list}
            columns={columns}
            idKey="id"
            hasHover
            emptyState={
              <EmptyState
                icon={<NoGroupsIcon />}
                title="No groups yet"
                description="Grant folders and connections to a group once, then add people to it."
                actions={newGroupButton}
              />
            }
          />
        )}
      </AsyncContent>

      <GroupDrawer
        state={drawer}
        onClose={() => setDrawer(CLOSED)}
        onSaved={saved => {
          setDrawer(CLOSED);
          showToast({body: `Saved ${saved.name}.`});
          groups.reload();
        }}
      />

      <ConfirmDeleteDialog
        target={deleting}
        title={g => `Delete ${g.name}?`}
        description="Its members lose the group's role and the connections granted to it. Their own access stays."
        onDelete={g => api.deleteGroup(g.id)}
        onClose={deleted => {
          if (deleted && deleting) {
            showToast({body: `Deleted ${deleting.name}.`});
            groups.reload();
          }
          setDeleting(null);
        }}
      />
    </PageFrame>
  );
}
