import {VStack, HStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Button} from '@astryxdesign/core/Button';
import {IconButton} from '@astryxdesign/core/IconButton';
import {Collapsible} from '@astryxdesign/core/Collapsible';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {PlusIcon, TrashIcon} from '@heroicons/react/24/outline';
import {
  extraParameter,
  isGuacdPortValid,
  otherStoredSecrets,
  type ConnectionForm,
  type ExtraParameter,
} from './connectionForm';
import ParameterInput, {type StoredSecret} from './ParameterInput';
import {isSensitive} from './secrets';

type Props = {
  form: ConnectionForm;
  onChange: (patch: Partial<ConnectionForm>) => void;
  storedSecret: (name: string) => StoredSecret | undefined;
};

/** guacd override and free parameters, collapsed unless already in use. */
export default function AdvancedSettings({form, onChange, storedSecret}: Props) {
  const otherSecrets = otherStoredSecrets(form);
  const isInUse = form.extras.length > 0 || form.guacdHost !== '' || otherSecrets.length > 0;
  return (
    <Collapsible defaultIsOpen={isInUse} trigger={<Heading level={3}>Advanced</Heading>}>
      <VStack gap={5} paddingBlockStart={3}>
        <VStack gap={4}>
          <Text color="secondary">
            Send this connection to another guacd or molcajeted instead of the gateway's default.
          </Text>
          <TextInput
            label="guacd host"
            value={form.guacdHost}
            onChange={guacdHost => onChange({guacdHost})}
            placeholder="Default"
          />
          <TextInput
            label="guacd port"
            value={form.guacdPort}
            onChange={guacdPort => onChange({guacdPort})}
            placeholder="Same as the gateway's guacd"
            inputMode="numeric"
            isDisabled={!form.guacdHost.trim()}
            status={isGuacdPortValid(form) ? undefined : {type: 'error', message: 'Enter a port number.'}}
          />
        </VStack>
        {otherSecrets.length > 0 && (
          <VStack gap={4}>
            {otherSecrets.map(name => (
              <ParameterInput
                key={name}
                field={{name, label: name, kind: 'password'}}
                value={form.fields[name] ?? ''}
                onChange={value => onChange({fields: {...form.fields, [name]: value}})}
                secret={storedSecret(name)}
              />
            ))}
          </VStack>
        )}
        <ExtraParameters extras={form.extras} onChange={extras => onChange({extras})} />
      </VStack>
    </Collapsible>
  );
}

type ExtraParametersProps = {
  extras: ExtraParameter[];
  onChange: (extras: ExtraParameter[]) => void;
};

function ExtraParameters({extras, onChange}: ExtraParametersProps) {
  const replace = (key: number, patch: Partial<ExtraParameter>) =>
    onChange(extras.map(e => (e.key === key ? {...e, ...patch} : e)));

  return (
    <VStack gap={3}>
      <VStack gap={1}>
        <Text weight="semibold">Other parameters</Text>
        <Text color="secondary">Any other guacd parameter, such as color-depth or enable-audio.</Text>
      </VStack>
      {extras.map(row => (
        <HStack key={row.key} gap={2} vAlign="end">
          <TextInput
            label="Parameter"
            value={row.name}
            onChange={name => replace(row.key, {name})}
            autoCapitalize="none"
            hasSpellCheck={false}
            width="100%"
          />
          <TextInput
            label="Value"
            value={row.value}
            onChange={value => replace(row.key, {value})}
            type={isSensitive(row.name) ? 'password' : 'text'}
            autoComplete={isSensitive(row.name) ? 'new-password' : 'off'}
            hasSpellCheck={false}
            width="100%"
          />
          <IconButton
            label={`Remove ${row.name || 'parameter'}`}
            icon={<TrashIcon width={16} height={16} />}
            variant="ghost"
            onClick={() => onChange(extras.filter(e => e.key !== row.key))}
          />
        </HStack>
      ))}
      <HStack>
        <Button
          label="Add parameter"
          size="sm"
          icon={<PlusIcon width={16} height={16} />}
          onClick={() => onChange([...extras, extraParameter()])}
        />
      </HStack>
    </VStack>
  );
}
