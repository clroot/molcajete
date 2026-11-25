import {useCallback, useState} from 'react';
import {MultiSelector} from '@astryxdesign/core/MultiSelector';
import {Text} from '@astryxdesign/core/Text';
import {api, type Access, type Group, type User} from '../api';
import {useApiData} from '../useApiData';
import AsyncContent from '../shell/AsyncContent';
import FormDialog from '../shell/FormDialog';

/** A connection or folder whose grants to edit. */
export type AccessTarget = {kind: 'connection' | 'folder'; id: string; name: string};

type Loaded = {access: Access; users: User[]; groups: Group[]};

type Props = {
  target: AccessTarget;
  onClose: () => void;
  onSaved: (target: AccessTarget) => void;
};

/** Who may open a connection, or everything in a folder. Mount it while open. */
export default function AccessDialog({target, onClose, onSaved}: Props) {
  const load = useCallback(async (): Promise<Loaded> => {
    const [access, users, groups] = await Promise.all([
      target.kind === 'connection' ? api.connectionAccess(target.id) : api.folderAccess(target.id),
      api.users(),
      api.groups(),
    ]);
    return {access, users, groups};
  }, [target]);
  const data = useApiData(load);
  const [edited, setEdited] = useState<Access | null>(null);
  const access = edited ?? (data.status === 'ready' ? data.data.access : null);

  const save = async () => {
    if (access === null) return;
    if (target.kind === 'connection') await api.setConnectionAccess(target.id, access);
    else await api.setFolderAccess(target.id, access);
    onSaved(target);
  };

  return (
    <FormDialog
      isOpen
      title={`Access to ${target.name}`}
      subtitle={
        target.kind === 'folder'
          ? 'Covers every connection in this folder and the folders inside it.'
          : 'Users and groups that can open this connection.'
      }
      submitLabel="Save"
      onClose={onClose}
      onSubmit={save}>
      <AsyncContent data={data} errorTitle="Could not load access">
        {({users, groups}) =>
          access && (
            <>
              <MultiSelector
                label="Users"
                value={access.users}
                onChange={ids => setEdited({...access, users: ids})}
                options={users.map(u => ({value: u.id, label: u.username}))}
                triggerDisplay="badges"
                placeholder="No users"
                hasSearch
                searchPlaceholder="Search users"
              />
              <MultiSelector
                label="Groups"
                value={access.groups}
                onChange={ids => setEdited({...access, groups: ids})}
                options={groups.map(g => ({value: g.id, label: g.name}))}
                triggerDisplay="badges"
                placeholder="No groups"
                hasSearch
                searchPlaceholder="Search groups"
                emptyText="There are no groups yet."
              />
              <Text type="supporting" color="secondary">
                Connection managers and administrators can open every connection.
              </Text>
            </>
          )
        }
      </AsyncContent>
    </FormDialog>
  );
}
