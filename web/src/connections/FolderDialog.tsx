import {useState} from 'react';
import {TextInput} from '@astryxdesign/core/TextInput';
import {api, type Folder} from '../api';
import FormDialog from '../shell/FormDialog';

/** Creates a folder inside `parentId`, or renames `folder`. */
export type FolderDialogState =
  {mode: 'create'; parentId: string | null; parentName?: string} | {mode: 'rename'; folder: Folder};

type Props = {
  state: FolderDialogState;
  onClose: () => void;
  onSaved: (folder: Folder) => void;
};

/** Mount it with a `key` per folder, while open. */
export default function FolderDialog({state, onClose, onSaved}: Props) {
  const [name, setName] = useState(state.mode === 'rename' ? state.folder.name : '');

  const save = async () => {
    if (!name.trim()) throw new Error('Give the folder a name.');
    const saved =
      state.mode === 'create'
        ? await api.createFolder(name.trim(), state.parentId)
        : await api.renameFolder(state.folder.id, name.trim());
    onSaved(saved);
  };

  const isNew = state.mode === 'create';
  return (
    <FormDialog
      isOpen
      title={isNew ? 'New folder' : 'Rename folder'}
      subtitle={state.mode === 'create' && state.parentName ? `In ${state.parentName}` : undefined}
      submitLabel={isNew ? 'Create folder' : 'Save'}
      onClose={onClose}
      onSubmit={save}>
      <TextInput label="Name" value={name} onChange={setName} isRequired hasAutoFocus />
    </FormDialog>
  );
}
