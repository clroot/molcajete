import {useState} from 'react';
import {TextInput} from '@astryxdesign/core/TextInput';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {api, type CreatedApiToken} from '../api';
import FormDialog from '../shell/FormDialog';
import {LIFETIMES, expiresAt, parseAllowedIps} from './format';

type Props = {
  onClose: () => void;
  onCreated: (token: CreatedApiToken) => void;
};

/** A new token for the signed-in user. Mount it while open. */
export default function CreateTokenDialog({onClose, onCreated}: Props) {
  const [name, setName] = useState('');
  const [lifetime, setLifetime] = useState('90');
  const [allowedIps, setAllowedIps] = useState('');

  const create = async () => {
    if (!name.trim()) throw new Error('Give the token a name.');
    const created = await api.createApiToken({
      name: name.trim(),
      expiresAt: expiresAt(lifetime),
      allowedIps: parseAllowedIps(allowedIps),
    });
    onCreated(created);
  };

  return (
    <FormDialog
      isOpen
      title="New API token"
      subtitle="Scripts and other applications use it to act as you, with your role and access."
      submitLabel="Create token"
      onClose={onClose}
      onSubmit={create}>
      <TextInput
        label="Name"
        description="What uses it, such as the inventory sync."
        value={name}
        onChange={setName}
        isRequired
        hasAutoFocus
      />
      <Selector
        label="Expires after"
        value={lifetime}
        onChange={value => value && setLifetime(value)}
        options={LIFETIMES}
      />
      <TextArea
        label="Allowed addresses"
        description="IP addresses or CIDR ranges, one per line. Leave empty to allow any address."
        placeholder={'10.0.0.0/8\n192.0.2.7'}
        value={allowedIps}
        onChange={setAllowedIps}
        isOptional
        hasSpellCheck={false}
      />
    </FormDialog>
  );
}
