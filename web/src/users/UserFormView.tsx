import {useState, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Switch} from '@astryxdesign/core/Switch';
import {Selector} from '@astryxdesign/core/Selector';
import {MultiSelector} from '@astryxdesign/core/MultiSelector';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {Text} from '@astryxdesign/core/Text';
import {api, errorMessage, type Role, type UserDetail} from '../api';
import {FormActions, FormDrawerLayout} from '../shell/FormDrawer';
import {MIN_PASSWORD_LENGTH, PASSWORD_RULE, isPasswordTooShort} from '../auth/password';
import {ROLES, roleLabel} from '../access/roles';
import GrantFields from '../access/GrantFields';
import type {Choices} from './UserDrawer';

const FORM_ID = 'user-form';

export type UserFormValues = {
  username: string;
  role: Role;
  disabled: boolean;
  /** Ids of the user's groups. */
  groups: string[];
  /** Ids of the connections granted directly. */
  connections: string[];
  /** Ids of the folders granted directly. */
  folders: string[];
};

type Props = {
  /** null for a new user. */
  userId: string | null;
  title: string;
  initial: UserFormValues;
  choices: Choices;
  /** The signed-in administrator cannot demote or disable themselves. */
  isSelf: boolean;
  onClose: () => void;
  onSaved: (user: UserDetail) => void;
};

const RANK: Record<Role, number> = {user: 0, connection_manager: 1, admin: 2};

export default function UserFormView({userId, title, initial, choices, isSelf, onClose, onSaved}: Props) {
  const isNew = userId === null;
  const [values, setValues] = useState(initial);
  const [password, setPassword] = useState('');
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');
  const update = (patch: Partial<UserFormValues>) => setValues(v => ({...v, ...patch}));

  // The highest role among the selected groups raises the user's own.
  const groupRole = choices.groups
    .filter(g => values.groups.includes(g.id))
    .reduce<Role>((best, g) => (RANK[g.role] > RANK[best] ? g.role : best), 'user');
  const effectiveRole = RANK[groupRole] > RANK[values.role] ? groupRole : values.role;

  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (!values.username.trim()) {
      setError('Enter a username.');
      return;
    }
    if ((isNew && !password) || isPasswordTooShort(password)) {
      setError(`The password needs at least ${MIN_PASSWORD_LENGTH} characters.`);
      return;
    }
    setError('');
    setIsSaving(true);
    const input = {...values, username: values.username.trim(), ...(password ? {password} : {})};
    try {
      onSaved(isNew ? await api.createUser(input) : await api.updateUser(userId, input));
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
          submitLabel={isNew ? 'Create user' : 'Save'}
          isSaving={isSaving}
          onCancel={onClose}
        />
      }>
      <form id={FORM_ID} onSubmit={save} noValidate>
        <VStack gap={5}>
          {error && <Banner status="error" title={error} />}
          <VStack gap={4}>
            <TextInput
              label="Username"
              value={values.username}
              onChange={username => update({username})}
              autoComplete="off"
              autoCapitalize="none"
              hasSpellCheck={false}
              hasAutoFocus={isNew}
              isRequired
            />
            <TextInput
              label={isNew ? 'Password' : 'New password'}
              type="password"
              value={password}
              onChange={setPassword}
              autoComplete="new-password"
              description={
                isNew
                  ? PASSWORD_RULE
                  : 'Leave empty to keep the current password. A new one signs the user out.'
              }
              status={isPasswordTooShort(password) ? {type: 'error', message: PASSWORD_RULE} : undefined}
              isRequired={isNew}
            />
          </VStack>

          <Divider />

          <VStack gap={4}>
            <Selector
              label="Role"
              value={values.role}
              onChange={role => role && update({role: role as Role})}
              options={ROLES}
              isDisabled={isSelf}
              disabledMessage="You cannot change your own role."
            />
            <MultiSelector
              label="Groups"
              description="The user also gets each group's role and access."
              value={values.groups}
              onChange={groups => update({groups})}
              options={choices.groups.map(g => ({
                value: g.id,
                label: g.name,
                description: roleLabel(g.role),
              }))}
              triggerDisplay="badges"
              placeholder="No groups"
              hasSearch
              searchPlaceholder="Search groups"
              emptyText="There are no groups yet."
            />
            {effectiveRole !== values.role && (
              <Text color="secondary">Through a group, this user's role is {roleLabel(effectiveRole)}.</Text>
            )}
            <Switch
              label="Disabled"
              description="Blocks sign-in and ends the user's sessions."
              value={values.disabled}
              onChange={disabled => update({disabled})}
              isDisabled={isSelf}
              disabledMessage="You cannot disable your own account."
            />
          </VStack>

          <Divider />

          {effectiveRole === 'user' ? (
            <GrantFields
              subject="this user"
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
