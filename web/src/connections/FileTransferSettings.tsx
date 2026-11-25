import {VStack} from '@astryxdesign/core/Stack';
import {Collapsible} from '@astryxdesign/core/Collapsible';
import {Heading} from '@astryxdesign/core/Heading';
import ParameterInput from './ParameterInput';
import {fileTransferFields, fileTransferSwitch} from './fileTransfer';

type Props = {
  protocol: string;
  fields: Record<string, string>;
  onChange: (name: string, value: string) => void;
};

/** The *File transfer* section, for protocols that have one; collapsed unless turned on. */
export default function FileTransferSettings({protocol, fields, onChange}: Props) {
  const enable = fileTransferSwitch(protocol);
  if (!enable) return null;
  const [toggle, ...details] = fileTransferFields(protocol);
  const isOn = fields[enable] === 'true';
  return (
    <Collapsible defaultIsOpen={isOn} trigger={<Heading level={3}>File transfer</Heading>}>
      <VStack gap={4} paddingBlockStart={3}>
        <ParameterInput
          field={toggle}
          value={fields[toggle.name] ?? ''}
          onChange={value => onChange(toggle.name, value)}
        />
        {isOn &&
          details.map(field => (
            <ParameterInput
              key={field.name}
              field={field}
              value={fields[field.name] ?? ''}
              onChange={value => onChange(field.name, value)}
            />
          ))}
      </VStack>
    </Collapsible>
  );
}
