import {VStack} from '@astryxdesign/core/Stack';
import {Collapsible} from '@astryxdesign/core/Collapsible';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import ParameterInput from './ParameterInput';
import {JUMP_HOST_FIELDS} from './jumpHosts';

type Props = {
  fields: Record<string, string>;
  onChange: (name: string, value: string) => void;
};

/** The *SSH jump hosts* section, collapsed unless a jump host is set. */
export default function JumpHostSettings({fields, onChange}: Props) {
  const isInUse = Boolean(fields['jump-hosts']);
  return (
    <Collapsible defaultIsOpen={isInUse} trigger={<Heading level={3}>SSH jump hosts</Heading>}>
      <VStack gap={4} paddingBlockStart={3}>
        <Text color="secondary">
          Reach a server that only a bastion can reach. Missing passwords are asked for when the session
          starts.
        </Text>
        {JUMP_HOST_FIELDS.map(field => (
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
