import {useState, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Switch} from '@astryxdesign/core/Switch';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {CheckboxList, CheckboxListItem} from '@astryxdesign/core/CheckboxList';
import {Text} from '@astryxdesign/core/Text';
import {api, errorMessage, type ConnectionSummary, type UserDetail} from '../api';
import {FormActions, FormDrawerLayout} from '../shell/FormDrawer';
import {MIN_PASSWORD_LENGTH, PASSWORD_RULE, isPasswordTooShort} from '../auth/password';
import {protocolLabel} from '../connections/protocols';

const FORM_ID = 'user-form';

export type UserFormValues = {
  username: string;
  isAdmin: boolean;
  disabled: boolean;
  /** Ids of the granted connections. */
  connections: string[];
};

type Props = {
  /** null for a new user. */
  userId: string | null;
  title: string;
  initial: UserFormValues;
  connections: ConnectionSummary[];
  /** The signed-in administrator cannot demote or disable themselves. */
  isSelf: boolean;
  onClose: () => void;
  onSaved: (user: UserDetail) => void;
};

export default function UserFormView({userId, title, initial, connections, isSelf, onClose, onSaved}: Props) {
  const isNew = userId === null;
  const [values, setValues] = useState(initial);
  const [password, setPassword] = useState('');
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');
  const update = (patch: Partial<UserFormValues>) => setValues(v => ({...v, ...patch}));

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
            <Switch
              label="Administrator"
              description="Manages connections and users, and can open every connection."
              value={values.isAdmin}
              onChange={isAdmin => update({isAdmin})}
              isDisabled={isSelf}
              disabledMessage="You cannot remove your own administrator access."
            />
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

          <GrantedConnections
            isAdmin={values.isAdmin}
            connections={connections}
            granted={values.connections}
            onChange={ids => update({connections: ids})}
          />
        </VStack>
      </form>
    </FormDrawerLayout>
  );
}

type GrantedConnectionsProps = {
  isAdmin: boolean;
  connections: ConnectionSummary[];
  granted: string[];
  onChange: (ids: string[]) => void;
};

function GrantedConnections({isAdmin, connections, granted, onChange}: GrantedConnectionsProps) {
  if (isAdmin) return <Text color="secondary">Administrators can open every connection.</Text>;
  if (connections.length === 0) return <Text color="secondary">There are no connections to grant yet.</Text>;
  return (
    <CheckboxList
      label="Connections"
      description="The connections this user can see and open."
      value={granted}
      onChange={onChange}
      hasDividers>
      {connections.map(c => (
        <CheckboxListItem
          key={c.id}
          value={c.id}
          label={c.name}
          description={[protocolLabel(c.protocol), c.hostname].filter(Boolean).join(' · ')}
        />
      ))}
    </CheckboxList>
  );
}
