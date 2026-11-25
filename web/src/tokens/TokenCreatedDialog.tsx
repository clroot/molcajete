import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {Banner} from '@astryxdesign/core/Banner';
import {Button} from '@astryxdesign/core/Button';
import {CodeBlock} from '@astryxdesign/core/CodeBlock';
import type {CreatedApiToken} from '../api';

type Props = {
  /** The token just created; the dialog is open while it is set. */
  created: CreatedApiToken | null;
  onClose: () => void;
};

/** Shows a new token the one time it can be seen. */
export default function TokenCreatedDialog({created, onClose}: Props) {
  return (
    <Dialog
      isOpen={created !== null}
      onOpenChange={open => !open && onClose()}
      purpose="required"
      width={560}>
      {created && (
        <Layout
          height="auto"
          header={<DialogHeader title={`Created ${created.name}`} />}
          content={
            <LayoutContent>
              <VStack gap={4}>
                <Banner
                  status="warning"
                  title="Copy the token now"
                  description="It is not shown again. Send it as Authorization: Bearer <token>."
                />
                <CodeBlock
                  code={created.token}
                  title="API token"
                  hasLanguageLabel={false}
                  width="100%"
                  isWrapped
                />
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack hAlign="end">
                <Button label="Done" variant="primary" onClick={onClose} />
              </HStack>
            </LayoutFooter>
          }
        />
      )}
    </Dialog>
  );
}
