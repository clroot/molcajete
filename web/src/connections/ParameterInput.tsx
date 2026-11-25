import type {ReactNode} from 'react';
import {TextInput} from '@astryxdesign/core/TextInput';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import {HStack} from '@astryxdesign/core/Stack';
import {IconButton} from '@astryxdesign/core/IconButton';
import {ArrowUturnLeftIcon, TrashIcon} from '@heroicons/react/24/outline';
import type {ParameterField} from './protocols';
import type {SecretState} from './connectionForm';
import {scaleOptionsWith} from '../session/displayScale';

// guacd reads boolean parameters as "true"; anything else is false.
const GUACD_TRUE = 'true';

/** A credential the gateway has stored, which the form can replace or clear but not show. */
export type StoredSecret = {
  state: Exclude<SecretState, 'none'>;
  onClear: () => void;
  onKeep: () => void;
};

type Props = {
  field: ParameterField;
  value: string;
  onChange: (value: string) => void;
  /** Set when the field is a stored credential. */
  secret?: StoredSecret;
};

const SECRET_PLACEHOLDER: Record<StoredSecret['state'], string> = {
  stored: 'Saved — type to replace',
  cleared: 'Removed when you save',
};

/** The input for one guacd parameter, chosen by the field's kind. */
export default function ParameterInput({field, value, onChange, secret}: Props) {
  const common = {label: field.label, description: field.description};
  const placeholder = secret ? SECRET_PLACEHOLDER[secret.state] : field.placeholder;
  const width = secret ? '100%' : undefined;
  switch (field.kind) {
    case 'switch':
      return (
        <Switch
          {...common}
          value={value === (field.onValue ?? GUACD_TRUE)}
          onChange={on => onChange(on ? (field.onValue ?? GUACD_TRUE) : '')}
        />
      );
    case 'select':
      return (
        <Selector
          {...common}
          value={value || null}
          hasClear
          placeholder={field.placeholder ?? 'Default'}
          onChange={v => onChange(v ?? '')}
          options={field.name === 'dpi' ? scaleOptionsWith(value) : (field.options ?? [])}
        />
      );
    case 'multiline':
      return (
        <WithSecretAction label={field.label} secret={secret}>
          <TextArea
            {...common}
            value={value}
            onChange={onChange}
            placeholder={placeholder}
            rows={4}
            hasSpellCheck={false}
            width={width}
          />
        </WithSecretAction>
      );
    case 'text':
    case 'password':
    case 'number':
      return (
        <WithSecretAction label={field.label} secret={secret}>
          <TextInput
            {...common}
            type={field.kind === 'password' ? 'password' : 'text'}
            inputMode={field.kind === 'number' ? 'numeric' : undefined}
            value={value}
            onChange={onChange}
            placeholder={placeholder}
            autoComplete={field.kind === 'password' ? 'new-password' : 'off'}
            autoCapitalize="none"
            hasSpellCheck={false}
            isRequired={field.isRequired}
            width={width}
          />
        </WithSecretAction>
      );
  }
}

/** Puts Clear (or Undo, once cleared) next to a stored credential's input. */
function WithSecretAction({
  label,
  secret,
  children,
}: {
  label: string;
  secret?: StoredSecret;
  children: ReactNode;
}) {
  if (!secret) return children;
  return (
    <HStack gap={2} vAlign="end">
      {children}
      {secret.state === 'stored' ? (
        <IconButton
          label={`Clear the saved ${label.toLowerCase()}`}
          tooltip={`Clear the saved ${label.toLowerCase()}`}
          variant="ghost"
          icon={<TrashIcon width={16} height={16} />}
          onClick={secret.onClear}
        />
      ) : (
        <IconButton
          label={`Keep the saved ${label.toLowerCase()}`}
          tooltip={`Keep the saved ${label.toLowerCase()}`}
          variant="ghost"
          icon={<ArrowUturnLeftIcon width={16} height={16} />}
          onClick={secret.onKeep}
        />
      )}
    </HStack>
  );
}
