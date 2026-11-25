import type {ReactNode} from 'react';
import {Layout, LayoutContent, LayoutHeader} from '@astryxdesign/core/Layout';
import {HStack} from '@astryxdesign/core/Stack';
import {Heading} from '@astryxdesign/core/Heading';
import {Text} from '@astryxdesign/core/Text';
import {VStack} from '@astryxdesign/core/VStack';

type Props = {
  title: string;
  description?: string;
  /** The page's primary action, end-aligned in the header. */
  action?: ReactNode;
  children: ReactNode;
};

/** A data page inside the app frame: pinned header, full-width content. */
export default function PageFrame({title, description, action, children}: Props) {
  return (
    <Layout
      height="fill"
      header={
        <LayoutHeader hasDivider padding={4}>
          <HStack gap={3} vAlign="center" hAlign="between">
            <VStack gap={0.5}>
              <Heading level={1}>{title}</Heading>
              {description && <Text color="secondary">{description}</Text>}
            </VStack>
            {action}
          </HStack>
        </LayoutHeader>
      }
      content={<LayoutContent padding={4}>{children}</LayoutContent>}
    />
  );
}
