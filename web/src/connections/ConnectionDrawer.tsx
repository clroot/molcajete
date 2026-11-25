import {useCallback} from 'react';
import {api, type Connection} from '../api';
import {FormDrawer, FormDrawerLayout} from '../shell/FormDrawer';
import AsyncContent from '../shell/AsyncContent';
import type {DrawerState} from '../shell/drawerState';
import {useApiData} from '../useApiData';
import {emptyConnectionForm, toConnectionForm} from './connectionForm';
import ConnectionFormView from './ConnectionFormView';

type Props = {
  state: DrawerState;
  onClose: () => void;
  onSaved: (connection: Connection) => void;
};

/** Creates a connection, or edits one once it has loaded. */
export default function ConnectionDrawer({state, onClose, onSaved}: Props) {
  return (
    <FormDrawer isOpen={state.mode !== 'closed'} title="Connection" width={480} onClose={onClose}>
      {state.mode === 'create' && (
        <ConnectionFormView isNew initial={emptyConnectionForm()} onClose={onClose} onSaved={onSaved} />
      )}
      {state.mode === 'edit' && (
        <EditConnection key={state.id} id={state.id} onClose={onClose} onSaved={onSaved} />
      )}
    </FormDrawer>
  );
}

type EditProps = {id: string; onClose: () => void; onSaved: (connection: Connection) => void};

function EditConnection({id, onClose, onSaved}: EditProps) {
  const fetchConnection = useCallback(() => api.connection(id), [id]);
  const connection = useApiData(fetchConnection);

  // The form mounts only with the loaded values, so nothing typed into it
  // can be replaced by a late response.
  if (connection.status !== 'ready') {
    return (
      <FormDrawerLayout title="Edit connection" subtitle={id} onClose={onClose}>
        <AsyncContent data={connection} errorTitle="Could not load the connection">
          {() => null}
        </AsyncContent>
      </FormDrawerLayout>
    );
  }
  return (
    <ConnectionFormView
      isNew={false}
      initial={toConnectionForm(connection.data)}
      onClose={onClose}
      onSaved={onSaved}
    />
  );
}
