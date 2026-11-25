import {useState, type FormEvent} from 'react';
import {VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Selector} from '@astryxdesign/core/Selector';
import {Banner} from '@astryxdesign/core/Banner';
import {Divider} from '@astryxdesign/core/Divider';
import {Heading} from '@astryxdesign/core/Heading';
import {api, errorMessage, type Connection, type Folder} from '../api';
import {FormActions, FormDrawerLayout} from '../shell/FormDrawer';
import {PROTOCOLS, protocolInfo, type ParameterField} from './protocols';
import {
  changeProtocol,
  clearSecret,
  keepSecret,
  secretState,
  slugify,
  toConnection,
  type ConnectionForm,
} from './connectionForm';
import ParameterInput, {type StoredSecret} from './ParameterInput';
import AdvancedSettings from './AdvancedSettings';
import {folderOptions} from './tree';
import RecordingSettings from './RecordingSettings';
import JumpHostSettings from './JumpHostSettings';
import FileTransferSettings from './FileTransferSettings';
import {fileTransferSwitch} from './fileTransfer';

const FORM_ID = 'connection-form';

type Props = {
  isNew: boolean;
  initial: ConnectionForm;
  folders: Folder[];
  onClose: () => void;
  onSaved: (connection: Connection) => void;
};

export default function ConnectionFormView({isNew, initial, folders, onClose, onSaved}: Props) {
  const [form, setForm] = useState(initial);
  const [isIdEdited, setIsIdEdited] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');

  const update = (patch: Partial<ConnectionForm>) => setForm(f => ({...f, ...patch}));
  // A new connection's id follows its name until the id is edited by hand.
  const changeName = (name: string) => update(isNew && !isIdEdited ? {name, id: slugify(name)} : {name});
  const protocol = protocolInfo(form.protocol);
  const storedSecret = (name: string): StoredSecret | undefined => {
    const state = secretState(form, name);
    if (state === 'none') return undefined;
    return {
      state,
      onClear: () => setForm(f => clearSecret(f, name)),
      onKeep: () => setForm(f => keepSecret(f, name)),
    };
  };
  const parameterInput = (field: ParameterField) => (
    <ParameterInput
      key={`${form.protocol}:${field.name}`}
      field={field}
      value={form.fields[field.name] ?? ''}
      onChange={value => update({fields: {...form.fields, [field.name]: value}})}
      secret={storedSecret(field.name)}
    />
  );

  const save = async (event: FormEvent) => {
    event.preventDefault();
    const result = toConnection(form);
    if (!result.ok) {
      setError(result.reason);
      return;
    }
    setError('');
    setIsSaving(true);
    try {
      const request = isNew ? api.createConnection : api.updateConnection;
      onSaved(await request(result.connection));
    } catch (err) {
      setError(errorMessage(err));
      setIsSaving(false);
    }
  };

  return (
    <FormDrawerLayout
      title={isNew ? 'New connection' : 'Edit connection'}
      subtitle={isNew ? undefined : form.id}
      onClose={onClose}
      footer={
        <FormActions
          formId={FORM_ID}
          submitLabel={isNew ? 'Create connection' : 'Save'}
          isSaving={isSaving}
          onCancel={onClose}
        />
      }>
      <form id={FORM_ID} onSubmit={save} noValidate>
        <VStack gap={5}>
          {error && <Banner status="error" title={error} />}

          <VStack gap={4}>
            <TextInput label="Name" value={form.name} onChange={changeName} isRequired hasAutoFocus={isNew} />
            {isNew && (
              <TextInput
                label="Id"
                description="Used in the connection's address. Letters, digits, '-', '_' and '.'."
                value={form.id}
                onChange={id => {
                  setIsIdEdited(true);
                  update({id});
                }}
                autoCapitalize="none"
                hasSpellCheck={false}
                isRequired
              />
            )}
            <Selector
              label="Protocol"
              value={form.protocol}
              onChange={value => value && setForm(f => changeProtocol(f, value, isNew))}
              options={PROTOCOLS.map(p => ({value: p.value, label: p.label}))}
            />
            {isNew && folders.length > 0 && (
              <Selector
                label="Folder"
                description="Users and groups granted the folder can open the connection."
                value={form.parentId}
                onChange={value => update({parentId: value ?? ''})}
                options={[{value: '', label: 'Top level'}, ...folderOptions(folders)]}
                hasSearch
                searchPlaceholder="Search folders"
              />
            )}
          </VStack>

          <Divider />

          <VStack gap={4}>
            <Heading level={3}>{protocol.label} settings</Heading>
            {protocol.fields.filter(field => !field.section).map(parameterInput)}
          </VStack>

          <Divider />

          <VStack gap={4}>
            <Heading level={3}>Clipboard and input</Heading>
            {protocol.fields.filter(field => field.section === 'input').map(parameterInput)}
          </VStack>

          <Divider />

          {fileTransferSwitch(form.protocol) && (
            <>
              <FileTransferSettings
                protocol={form.protocol}
                fields={form.fields}
                onChange={(name, value) => update({fields: {...form.fields, [name]: value}})}
              />
              <Divider />
            </>
          )}

          <JumpHostSettings
            fields={form.fields}
            onChange={(name, value) => update({fields: {...form.fields, [name]: value}})}
            storedSecret={storedSecret}
          />

          <Divider />

          <RecordingSettings recording={form.recording} onChange={recording => update({recording})} />

          <Divider />

          <AdvancedSettings form={form} onChange={update} storedSecret={storedSecret} />
        </VStack>
      </form>
    </FormDrawerLayout>
  );
}
