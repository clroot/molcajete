import {useState, type FormEvent} from 'react';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Button} from '@astryxdesign/core/Button';
import {requiredLabel} from '../connections/jumpHosts';

type Props = {
  /** Parameter names the server asked for, e.g. ["username", "password"]. */
  parameters: string[];
  connectionName: string;
  onSubmit: (values: Record<string, string>) => void;
  onCancel: () => void;
};

const isSecret = (parameter: string) => /password|passphrase/.test(parameter);

/** Asks for the values the remote server still needs (`required`). */
export default function CredentialsDialog({parameters, connectionName, onSubmit, onCancel}: Props) {
  const [values, setValues] = useState<Record<string, string>>({});

  const submit = (event: FormEvent) => {
    event.preventDefault();
    onSubmit(Object.fromEntries(parameters.map(p => [p, values[p] ?? ''])));
  };

  return (
    <Dialog isOpen onOpenChange={() => {}} purpose="required" width={420}>
      <form onSubmit={submit}>
        <Layout
          height="auto"
          header={<DialogHeader title="Credentials required" subtitle={connectionName} />}
          content={
            <LayoutContent>
              <VStack gap={4}>
                {parameters.map((parameter, i) => (
                  <TextInput
                    key={parameter}
                    label={requiredLabel(parameter)}
                    type={isSecret(parameter) ? 'password' : 'text'}
                    value={values[parameter] ?? ''}
                    onChange={value => setValues(v => ({...v, [parameter]: value}))}
                    autoComplete="off"
                    autoCapitalize="none"
                    hasAutoFocus={i === 0}
                  />
                ))}
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack gap={2} hAlign="end">
                <Button label="Cancel" variant="secondary" onClick={onCancel} />
                <Button label="Continue" variant="primary" type="submit" />
              </HStack>
            </LayoutFooter>
          }
        />
      </form>
    </Dialog>
  );
}
