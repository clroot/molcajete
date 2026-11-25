// Built from the Astryx `DrawerForm` block: header, scrolling fields and
// pinned footer actions; purpose="form" keeps Escape but ignores scrim clicks.
import {useEffect, useState, type FormEvent} from 'react';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Button} from '@astryxdesign/core/Button';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {Banner} from '@astryxdesign/core/Banner';
import {Collapsible} from '@astryxdesign/core/Collapsible';
import {Divider} from '@astryxdesign/core/Divider';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {Spinner} from '@astryxdesign/core/Spinner';
import {Center} from '@astryxdesign/core/Center';
import {PlusIcon, TrashIcon} from '@heroicons/react/24/outline';
import {api, errorMessage, type Connection} from '../api';
import {PROTOCOLS, protocolInfo, type ParameterField} from '../protocols';

type Extra = {key: number; name: string; value: string};

type Form = {
  id: string;
  name: string;
  protocol: string;
  guacdHost: string;
  guacdPort: string;
  /** Values of the fields the current protocol shows. */
  fields: Record<string, string>;
  /** Every other guacd parameter. */
  extras: Extra[];
};

let nextExtraKey = 1;
const extra = (name: string, value: string): Extra => ({key: nextExtraKey++, name, value});

/** Splits parameters into the protocol's own fields and free extras. */
function splitParameters(protocol: string, parameters: Record<string, string>) {
  const known = new Set(protocolInfo(protocol).fields.map(f => f.name));
  const fields: Record<string, string> = {};
  const extras: Extra[] = [];
  for (const [name, value] of Object.entries(parameters)) {
    if (known.has(name)) fields[name] = value;
    else extras.push(extra(name, value));
  }
  return {fields, extras};
}

function joinParameters(form: Form): Record<string, string> {
  const parameters: Record<string, string> = {};
  for (const {name, value} of form.extras) {
    if (name.trim() && value !== '') parameters[name.trim()] = value;
  }
  for (const [name, value] of Object.entries(form.fields)) {
    if (value !== '') parameters[name] = value;
  }
  return parameters;
}

function emptyForm(): Form {
  return {id: '', name: '', protocol: 'vnc', guacdHost: '', guacdPort: '', fields: {}, extras: []};
}

function toForm(c: Connection): Form {
  return {
    id: c.id,
    name: c.name,
    protocol: c.protocol,
    guacdHost: c.guacdHost ?? '',
    guacdPort: c.guacdPort == null ? '' : String(c.guacdPort),
    ...splitParameters(c.protocol, c.parameters),
  };
}

/** A URL-safe id derived from the name, for new connections. */
function slug(name: string) {
  return name
    .toLowerCase()
    .normalize('NFKD')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64);
}

type Props = {
  /** undefined: closed; null: create; otherwise the id to edit. */
  connectionId: string | null | undefined;
  onClose: () => void;
  onSaved: (connection: Connection) => void;
};

export default function ConnectionDrawer({connectionId, onClose, onSaved}: Props) {
  const isOpen = connectionId !== undefined;
  const isNew = connectionId === null;
  const [form, setForm] = useState<Form>(emptyForm);
  const [idEdited, setIdEdited] = useState(false);
  const [isLoading, setIsLoading] = useState(false);
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');

  useEffect(() => {
    setError('');
    setIdEdited(false);
    if (connectionId === undefined) return;
    if (connectionId === null) {
      setForm(emptyForm());
      return;
    }
    let cancelled = false;
    setIsLoading(true);
    api
      .connection(connectionId)
      .then(c => !cancelled && setForm(toForm(c)))
      .catch(err => !cancelled && setError(errorMessage(err)))
      .finally(() => !cancelled && setIsLoading(false));
    return () => {
      cancelled = true;
    };
  }, [connectionId]);

  const update = (patch: Partial<Form>) => setForm(f => ({...f, ...patch}));
  const setField = (name: string, value: string) =>
    setForm(f => ({...f, fields: {...f.fields, [name]: value}}));

  const changeProtocol = (protocol: string) =>
    setForm(f => ({...f, protocol, ...splitParameters(protocol, joinParameters(f))}));

  const protocol = protocolInfo(form.protocol);
  const portField = form.guacdPort.trim();
  const guacdPortInvalid = portField !== '' && !/^\d{1,5}$/.test(portField);

  const save = async (event?: FormEvent) => {
    event?.preventDefault();
    setError('');
    if (!form.name.trim()) {
      setError('Give the connection a name.');
      return;
    }
    if (guacdPortInvalid) return;
    const connection: Connection = {
      id: form.id.trim(),
      name: form.name.trim(),
      protocol: form.protocol,
      guacdHost: form.guacdHost.trim() || null,
      guacdPort: form.guacdHost.trim() && portField ? Number(portField) : null,
      parameters: joinParameters(form),
    };
    setIsSaving(true);
    try {
      onSaved(isNew ? await api.createConnection(connection) : await api.updateConnection(connection));
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setIsSaving(false);
    }
  };

  const title = isNew ? 'New connection' : 'Edit connection';

  return (
    <Drawer
      isOpen={isOpen}
      onOpenChange={open => !open && onClose()}
      label={title}
      purpose="form"
      width={480}>
      <Layout
        header={<DrawerHeader title={title} subtitle={isNew ? undefined : form.id} onOpenChange={() => onClose()} />}
        content={
          <LayoutContent>
            {isLoading ? (
              <Center axis="both" padding={10}>
                <Spinner size="lg" />
              </Center>
            ) : (
              <form id="connection-form" onSubmit={save} noValidate>
                <VStack gap={5}>
                  {error && <Banner status="error" title={error} />}

                  <VStack gap={4}>
                    <TextInput
                      label="Name"
                      value={form.name}
                      onChange={name =>
                        update(isNew && !idEdited ? {name, id: slug(name)} : {name})
                      }
                      isRequired
                      hasAutoFocus={isNew}
                    />
                    {isNew && (
                      <TextInput
                        label="Id"
                        description="Used in the connection's address. Letters, digits, '-', '_' and '.'."
                        value={form.id}
                        onChange={id => {
                          setIdEdited(true);
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
                      onChange={value => value && changeProtocol(value)}
                      options={PROTOCOLS.map(p => ({value: p.value, label: p.label}))}
                    />
                  </VStack>

                  <Divider />

                  <VStack gap={4}>
                    <Heading level={3}>{protocol.label} settings</Heading>
                    {protocol.fields.map(field => (
                      <ParameterInput
                        key={`${form.protocol}:${field.name}`}
                        field={field}
                        value={form.fields[field.name] ?? ''}
                        placeholder={field.name === 'port' ? protocol.defaultPort : field.placeholder}
                        onChange={value => setField(field.name, value)}
                      />
                    ))}
                  </VStack>

                  <Divider />

                  <Collapsible
                    defaultIsOpen={form.extras.length > 0 || form.guacdHost !== ''}
                    trigger={<Heading level={3}>Advanced</Heading>}>
                    <VStack gap={5} paddingBlockStart={3}>
                      <VStack gap={4}>
                        <Text color="secondary">
                          Send this connection to another guacd or molcajeted instead of the gateway's default.
                        </Text>
                        <TextInput
                          label="guacd host"
                          value={form.guacdHost}
                          onChange={guacdHost => update({guacdHost})}
                          placeholder="Default"
                        />
                        <TextInput
                          label="guacd port"
                          value={form.guacdPort}
                          onChange={guacdPort => update({guacdPort})}
                          placeholder="4822"
                          inputMode="numeric"
                          isDisabled={!form.guacdHost.trim()}
                          status={guacdPortInvalid ? {type: 'error', message: 'Enter a port number.'} : undefined}
                        />
                      </VStack>

                      <VStack gap={3}>
                        <VStack gap={1}>
                          <Text weight="semibold">Other parameters</Text>
                          <Text color="secondary">
                            Any other guacd parameter, such as color-depth or enable-audio.
                          </Text>
                        </VStack>
                        {form.extras.map(row => (
                          <HStack key={row.key} gap={2} vAlign="end">
                            <TextInput
                              label="Parameter"
                              value={row.name}
                              onChange={name =>
                                update({extras: form.extras.map(e => (e.key === row.key ? {...e, name} : e))})
                              }
                              autoCapitalize="none"
                              hasSpellCheck={false}
                              width="100%"
                            />
                            <TextInput
                              label="Value"
                              value={row.value}
                              onChange={value =>
                                update({extras: form.extras.map(e => (e.key === row.key ? {...e, value} : e))})
                              }
                              hasSpellCheck={false}
                              width="100%"
                            />
                            <IconButton
                              label={`Remove ${row.name || 'parameter'}`}
                              icon={<TrashIcon width={16} height={16} />}
                              variant="ghost"
                              onClick={() => update({extras: form.extras.filter(e => e.key !== row.key)})}
                            />
                          </HStack>
                        ))}
                        <HStack>
                          <Button
                            label="Add parameter"
                            size="sm"
                            icon={<PlusIcon width={16} height={16} />}
                            onClick={() => update({extras: [...form.extras, extra('', '')]})}
                          />
                        </HStack>
                      </VStack>
                    </VStack>
                  </Collapsible>
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
                label={isNew ? 'Create connection' : 'Save'}
                variant="primary"
                type="submit"
                form="connection-form"
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

type ParameterInputProps = {
  field: ParameterField;
  value: string;
  placeholder?: string;
  onChange: (value: string) => void;
};

function ParameterInput({field, value, placeholder, onChange}: ParameterInputProps) {
  switch (field.kind) {
    case 'switch':
      return (
        <Switch
          label={field.label}
          description={field.description}
          value={value === 'true'}
          onChange={checked => onChange(checked ? 'true' : '')}
        />
      );
    case 'select':
      return (
        <Selector
          label={field.label}
          description={field.description}
          value={value || null}
          hasClear
          placeholder="Default"
          onChange={v => onChange(v ?? '')}
          options={field.options ?? []}
        />
      );
    case 'multiline':
      return (
        <TextArea
          label={field.label}
          description={field.description}
          value={value}
          onChange={onChange}
          placeholder={placeholder}
          rows={4}
          hasSpellCheck={false}
        />
      );
    default:
      return (
        <TextInput
          label={field.label}
          description={field.description}
          type={field.kind === 'password' ? 'password' : 'text'}
          inputMode={field.kind === 'number' ? 'numeric' : undefined}
          value={value}
          onChange={onChange}
          placeholder={placeholder}
          autoComplete={field.kind === 'password' ? 'new-password' : 'off'}
          autoCapitalize="none"
          hasSpellCheck={false}
          isRequired={field.name === 'hostname'}
        />
      );
  }
}
