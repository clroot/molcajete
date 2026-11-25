import {useState, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Selector} from '@astryxdesign/core/Selector';
import {MultiSelector} from '@astryxdesign/core/MultiSelector';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {Text} from '@astryxdesign/core/Text';
import {api, errorMessage, type GroupDetail, type Role} from '../api';
import {FormActions, FormDrawerLayout} from '../shell/FormDrawer';
import {ROLES} from '../access/roles';
import GrantFields from '../access/GrantFields';
import type {GroupChoices} from './GroupDrawer';

const FORM_ID = 'group-form';

export type GroupFormValues = {
  name: string;
  role: Role;
  /** Ids of the members. */
  members: string[];
  /** Ids of the connections granted. */
  connections: string[];
  /** Ids of the folders granted. */
  folders: string[];
};

type Props = {
  /** null for a new group. */
  groupId: string | null;
  title: string;
  initial: GroupFormValues;
  choices: GroupChoices;
  onClose: () => void;
  onSaved: (group: GroupDetail) => void;
};

export default function GroupFormView({groupId, title, initial, choices, onClose, onSaved}: Props) {
  const isNew = groupId === null;
  const [values, setValues] = useState(initial);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');
  const update = (patch: Partial<GroupFormValues>) => setValues(v => ({...v, ...patch}));

  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (!values.name.trim()) {
      setError('Give the group a name.');
      return;
    }
    setError('');
    setIsSaving(true);
    const input = {...values, name: values.name.trim()};
    try {
      onSaved(isNew ? await api.createGroup(input) : await api.updateGroup(groupId, input));
    } catch (err) {
      setError(errorMessage(err));
      setIsSaving(false);
    }
  };

  return (
    <FormDrawerLayout
      title={title}
      onClose={onClose}
      footer={
        <FormActions
          formId={FORM_ID}
          submitLabel={isNew ? 'Create group' : 'Save'}
          isSaving={isSaving}
          onCancel={onClose}
        />
      }>
      <form id={FORM_ID} onSubmit={save} noValidate>
        <VStack gap={5}>
          {error && <Banner status="error" title={error} />}
          <VStack gap={4}>
            <TextInput
              label="Name"
              value={values.name}
              onChange={name => update({name})}
              autoComplete="off"
              hasAutoFocus={isNew}
              isRequired
            />
            <Selector
              label="Role"
              description="Members get this role when it is higher than their own."
              value={values.role}
              onChange={role => role && update({role: role as Role})}
              options={ROLES}
            />
            <MultiSelector
              label="Members"
              value={values.members}
              onChange={members => update({members})}
              options={choices.users.map(u => ({value: u.id, label: u.username}))}
              triggerDisplay="badges"
              placeholder="No members"
              hasSearch
              searchPlaceholder="Search users"
            />
          </VStack>

          <Divider />

          {values.role === 'user' ? (
            <GrantFields
              subject="the group's members"
              grants={values}
              folders={choices.folders}
              connections={choices.connections}
              onChange={({connections, folders}) => update({connections, folders})}
            />
          ) : (
            <Text color="secondary">Connection managers and administrators can open every connection.</Text>
          )}
        </VStack>
      </form>
    </FormDrawerLayout>
  );
}
