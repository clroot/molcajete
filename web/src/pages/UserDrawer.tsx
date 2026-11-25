// Built from the Astryx `DrawerForm` block.
import {useEffect, useState, type FormEvent} from 'react';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Button} from '@astryxdesign/core/Button';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Switch} from '@astryxdesign/core/Switch';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {CheckboxList, CheckboxListItem} from '@astryxdesign/core/CheckboxList';
import {Text} from '@astryxdesign/core/Text';
import {Spinner} from '@astryxdesign/core/Spinner';
import {Center} from '@astryxdesign/core/Center';
import {api, errorMessage, type ConnectionSummary, type UserDetail, type UserInput} from '../api';
import {protocolLabel} from '../protocols';

type Props = {
  /** undefined: closed; null: create; otherwise the id to edit. */
  userId: string | null | undefined;
  currentUserId: string;
  onClose: () => void;
  onSaved: (user: UserDetail) => void;
};

export default function UserDrawer({userId, currentUserId, onClose, onSaved}: Props) {
  const isOpen = userId !== undefined;
  const isNew = userId === null;
  const isSelf = userId === currentUserId;
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [isAdmin, setIsAdmin] = useState(false);
  const [disabled, setDisabled] = useState(false);
  const [granted, setGranted] = useState<string[]>([]);
  const [connections, setConnections] = useState<ConnectionSummary[]>([]);
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    setError('');
    setPassword('');
    if (userId === undefined) return;
    let cancelled = false;
    setIsLoading(true);
    const user = userId === null ? Promise.resolve(null) : api.user(userId);
    Promise.all([user, api.connections()])
      .then(([u, list]) => {
        if (cancelled) return;
        setConnections(list);
        setUsername(u?.username ?? '');
        setIsAdmin(u?.isAdmin ?? false);
        setDisabled(u?.disabled ?? false);
        setGranted(u?.connections ?? []);
      })
      .catch(err => !cancelled && setError(errorMessage(err)))
      .finally(() => !cancelled && setIsLoading(false));
    return () => {
      cancelled = true;
    };
  }, [userId]);

  const passwordTooShort = password !== '' && password.length < 8;

  const save = async (event?: FormEvent) => {
    event?.preventDefault();
    setError('');
    if (!username.trim()) {
      setError('Enter a username.');
      return;
    }
    if ((isNew && !password) || passwordTooShort) {
      setError('Passwords need at least 8 characters.');
      return;
    }
    const input: UserInput = {
      username: username.trim(),
      isAdmin,
      disabled,
      connections: granted,
      ...(password ? {password} : {}),
    };
    setIsSaving(true);
    try {
      onSaved(isNew ? await api.createUser(input) : await api.updateUser(userId!, input));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setIsSaving(false);
    }
  };

  const title = isNew ? 'New user' : 'Edit user';

  return (
    <Drawer isOpen={isOpen} onOpenChange={open => !open && onClose()} label={title} purpose="form" width={440}>
      <Layout
        header={<DrawerHeader title={title} onOpenChange={() => onClose()} />}
        content={
          <LayoutContent>
            {isLoading ? (
              <Center axis="both" padding={10}>
                <Spinner size="lg" />
              </Center>
            ) : (
              <form id="user-form" onSubmit={save} noValidate>
                <VStack gap={5}>
                  {error && <Banner status="error" title={error} />}
                  <VStack gap={4}>
                    <TextInput
                      label="Username"
                      value={username}
                      onChange={setUsername}
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
                          ? 'At least 8 characters.'
                          : 'Leave empty to keep the current password. A new one signs the user out.'
                      }
                      status={passwordTooShort ? {type: 'error', message: 'At least 8 characters.'} : undefined}
                      isRequired={isNew}
                    />
                  </VStack>

                  <Divider />

                  <VStack gap={4}>
                    <Switch
                      label="Administrator"
                      description="Manages connections and users, and can open every connection."
                      value={isAdmin}
                      onChange={setIsAdmin}
                      isDisabled={isSelf}
                      disabledMessage="You cannot remove your own administrator access."
                    />
                    <Switch
                      label="Disabled"
                      description="Blocks sign-in and ends the user's sessions."
                      value={disabled}
                      onChange={setDisabled}
                      isDisabled={isSelf}
                      disabledMessage="You cannot disable your own account."
                    />
                  </VStack>

                  <Divider />

                  {isAdmin ? (
                    <Text color="secondary">Administrators can open every connection.</Text>
                  ) : connections.length === 0 ? (
                    <Text color="secondary">There are no connections to grant yet.</Text>
                  ) : (
                    <CheckboxList
                      label="Connections"
                      description="The connections this user can see and open."
                      value={granted}
                      onChange={setGranted}
                      hasDividers>
                      {connections.map(c => (
                        <CheckboxListItem
                          key={c.id}
                          value={c.id}
                          label={c.name}
                          description={`${protocolLabel(c.protocol)}${c.hostname ? ` · ${c.hostname}` : ''}`}
                        />
                      ))}
                    </CheckboxList>
                  )}
                </VStack>
              </form>
            )}
          </LayoutContent>
        }
        footer={
          <LayoutFooter>
            <HStack gap={2} hAlign="end">
              <Button label="Cancel" variant="secondary" onClick={onClose} />
              <Button
                label={isNew ? 'Create user' : 'Save'}
                variant="primary"
                type="submit"
                form="user-form"
                isLoading={isSaving}
                isDisabled={isLoading}
              />
            </HStack>
          </LayoutFooter>
        }
      />
    </Drawer>
  );
}
