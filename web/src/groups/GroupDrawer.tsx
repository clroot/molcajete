import {useCallback} from 'react';
import {api, type ConnectionSummary, type Folder, type GroupDetail, type User} from '../api';
import {FormDrawer, FormDrawerLayout} from '../shell/FormDrawer';
import AsyncContent from '../shell/AsyncContent';
import type {DrawerState} from '../shell/drawerState';
import {useApiData} from '../useApiData';
import GroupFormView, {type GroupFormValues} from './GroupFormView';

type Props = {
  state: DrawerState;
  onClose: () => void;
  onSaved: (group: GroupDetail) => void;
};

const NEW_GROUP: GroupFormValues = {name: '', role: 'user', members: [], connections: [], folders: []};

export type GroupChoices = {users: User[]; connections: ConnectionSummary[]; folders: Folder[]};

type Loaded = {values: GroupFormValues; choices: GroupChoices};

/** Creates a group, or edits one; the form mounts once its data has loaded. */
export default function GroupDrawer({state, onClose, onSaved}: Props) {
  const groupId = state.mode === 'edit' ? state.id : null;
  return (
    <FormDrawer isOpen={state.mode !== 'closed'} title="Group" width={480} onClose={onClose}>
      {state.mode !== 'closed' && (
        <LoadedGroupForm key={groupId ?? 'new'} groupId={groupId} onClose={onClose} onSaved={onSaved} />
      )}
    </FormDrawer>
  );
}

type LoadedGroupFormProps = {
  /** null for a new group. */
  groupId: string | null;
  onClose: () => void;
  onSaved: (group: GroupDetail) => void;
};

function LoadedGroupForm({groupId, onClose, onSaved}: LoadedGroupFormProps) {
  const load = useCallback(async (): Promise<Loaded> => {
    const [group, users, connections, folders] = await Promise.all([
      groupId === null ? null : api.group(groupId),
      api.users(),
      api.connections(),
      api.folders(),
    ]);
    const values = group
      ? {
          name: group.name,
          role: group.role,
          members: group.members,
          connections: group.connections,
          folders: group.folders,
        }
      : NEW_GROUP;
    return {values, choices: {users, connections, folders}};
  }, [groupId]);
  const data = useApiData(load);
  const title = groupId === null ? 'New group' : 'Edit group';

  if (data.status !== 'ready') {
    return (
      <FormDrawerLayout title={title} onClose={onClose}>
        <AsyncContent data={data} errorTitle="Could not load the group">
          {() => null}
        </AsyncContent>
      </FormDrawerLayout>
    );
  }
  return (
    <GroupFormView
      groupId={groupId}
      title={title}
      initial={data.data.values}
      choices={data.data.choices}
      onClose={onClose}
      onSaved={onSaved}
    />
  );
}
