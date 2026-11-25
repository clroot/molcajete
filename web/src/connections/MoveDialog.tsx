import {useState} from 'react';
import {Selector} from '@astryxdesign/core/Selector';
import {api, type Folder} from '../api';
import FormDialog from '../shell/FormDialog';
import {folderAndDescendants, folderOptions} from './tree';

/** A connection or folder to move. */
export type MoveTarget = {kind: 'connection' | 'folder'; id: string; name: string; parentId: string | null};

const TOP_LEVEL = '';

type Props = {
  target: MoveTarget;
  folders: Folder[];
  onClose: () => void;
  onMoved: (target: MoveTarget, folderName: string) => void;
};

/** Mount it with a `key` per target, while open. */
export default function MoveDialog({target, folders, onClose, onMoved}: Props) {
  const [destination, setDestination] = useState(target.parentId ?? TOP_LEVEL);

  // A folder cannot go into itself or a folder inside it.
  const excluded = target.kind === 'folder' ? folderAndDescendants(folders, target.id) : new Set<string>();
  const options = [{value: TOP_LEVEL, label: 'Top level'}, ...folderOptions(folders, excluded)];

  const move = async () => {
    const parentId = destination === TOP_LEVEL ? null : destination;
    if (target.kind === 'connection') await api.moveConnection(target.id, parentId);
    else await api.moveFolder(target.id, parentId);
    onMoved(target, options.find(o => o.value === destination)?.label ?? '');
  };

  return (
    <FormDialog
      isOpen
      title={`Move ${target.name}`}
      subtitle={target.kind === 'folder' ? 'Everything in the folder moves with it.' : undefined}
      submitLabel="Move"
      onClose={onClose}
      onSubmit={move}>
      <Selector
        label="Move to"
        description="Users and groups granted the new folder can open it; grants on the folders it leaves stop applying."
        value={destination}
        onChange={value => setDestination(value ?? TOP_LEVEL)}
        options={options}
        hasSearch
        searchPlaceholder="Search folders"
      />
    </FormDialog>
  );
}
