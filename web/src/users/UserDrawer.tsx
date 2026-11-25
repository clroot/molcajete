import {useCallback} from 'react';
import {api, type ConnectionSummary, type Folder, type Group, type UserDetail} from '../api';
import {FormDrawer, FormDrawerLayout} from '../shell/FormDrawer';
import AsyncContent from '../shell/AsyncContent';
import type {DrawerState} from '../shell/drawerState';
import {useApiData} from '../useApiData';
import UserFormView, {type UserFormValues} from './UserFormView';

type Props = {
  state: DrawerState;
  currentUserId: string;
  onClose: () => void;
  onSaved: (user: UserDetail) => void;
};

const NEW_USER: UserFormValues = {
  username: '',
  role: 'user',
  disabled: false,
  groups: [],
  connections: [],
  folders: [],
};

export type Choices = {connections: ConnectionSummary[]; folders: Folder[]; groups: Group[]};

type Loaded = {values: UserFormValues; choices: Choices};

/** Creates a user, or edits one; the form mounts once its data has loaded. */
export default function UserDrawer({state, currentUserId, onClose, onSaved}: Props) {
  const userId = state.mode === 'edit' ? state.id : null;
  return (
    <FormDrawer isOpen={state.mode !== 'closed'} title="User" width={480} onClose={onClose}>
      {state.mode !== 'closed' && (
        <LoadedUserForm
          key={userId ?? 'new'}
          userId={userId}
          isSelf={userId === currentUserId}
          onClose={onClose}
          onSaved={onSaved}
        />
      )}
    </FormDrawer>
  );
}

type LoadedUserFormProps = {
  /** null for a new user. */
  userId: string | null;
  isSelf: boolean;
  onClose: () => void;
  onSaved: (user: UserDetail) => void;
};

function LoadedUserForm({userId, isSelf, onClose, onSaved}: LoadedUserFormProps) {
  const load = useCallback(async (): Promise<Loaded> => {
    const [user, connections, folders, groups] = await Promise.all([
      userId === null ? null : api.user(userId),
      api.connections(),
      api.folders(),
      api.groups(),
    ]);
    const values = user
      ? {
          username: user.username,
          role: user.role,
          disabled: user.disabled,
          groups: user.groups,
          connections: user.connections,
          folders: user.folders,
        }
      : NEW_USER;
    return {values, choices: {connections, folders, groups}};
  }, [userId]);
  const data = useApiData(load);
  const title = userId === null ? 'New user' : 'Edit user';

  if (data.status !== 'ready') {
    return (
      <FormDrawerLayout title={title} onClose={onClose}>
        <AsyncContent data={data} errorTitle="Could not load the user">
          {() => null}
        </AsyncContent>
      </FormDrawerLayout>
    );
  }
  return (
    <UserFormView
      userId={userId}
      title={title}
      initial={data.data.values}
      choices={data.data.choices}
      isSelf={isSelf}
      onClose={onClose}
      onSaved={onSaved}
    />
  );
}
