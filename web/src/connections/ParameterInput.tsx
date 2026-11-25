import {TextInput} from '@astryxdesign/core/TextInput';
import {TextArea} from '@astryxdesign/core/TextArea';
import {Selector} from '@astryxdesign/core/Selector';
import {Switch} from '@astryxdesign/core/Switch';
import type {ParameterField} from './protocols';
import {scaleOptionsWith} from '../session/displayScale';

// guacd reads boolean parameters as "true"; anything else is false.
const GUACD_TRUE = 'true';

type Props = {
  field: ParameterField;
  value: string;
  onChange: (value: string) => void;
};

/** The input for one guacd parameter, chosen by the field's kind. */
export default function ParameterInput({field, value, onChange}: Props) {
  const common = {label: field.label, description: field.description};
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
        <TextArea
          {...common}
          value={value}
          onChange={onChange}
          placeholder={field.placeholder}
          rows={4}
          hasSpellCheck={false}
        />
      );
    case 'text':
    case 'password':
    case 'number':
      return (
        <TextInput
          {...common}
          type={field.kind === 'password' ? 'password' : 'text'}
          inputMode={field.kind === 'number' ? 'numeric' : undefined}
          value={value}
          onChange={onChange}
          placeholder={field.placeholder}
          autoComplete={field.kind === 'password' ? 'new-password' : 'off'}
          autoCapitalize="none"
          hasSpellCheck={false}
          isRequired={field.isRequired}
        />
      );
  }
}
