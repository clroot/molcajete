// The Astryx `DrawerForm` block: header, scrolling fields and pinned footer
// actions; purpose="form" keeps Escape but ignores scrim clicks.
import type {ReactNode} from 'react';
import {Drawer, DrawerHeader} from '@astryxdesign/core/Drawer';
import {Button} from '@astryxdesign/core/Button';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack} from '@astryxdesign/core/Stack';

type FormDrawerProps = {
  isOpen: boolean;
  title: string;
  width: number;
  onClose: () => void;
  children: ReactNode;
};

export function FormDrawer({isOpen, title, width, onClose, children}: FormDrawerProps) {
  return (
    <Drawer
      isOpen={isOpen}
      onOpenChange={open => !open && onClose()}
      label={title}
      purpose="form"
      width={width}>
      {children}
    </Drawer>
  );
}

type FormDrawerLayoutProps = {
  title: string;
  subtitle?: string;
  onClose: () => void;
  /** Footer actions; omit while there is nothing to submit yet. */
  footer?: ReactNode;
  children: ReactNode;
};

export function FormDrawerLayout({title, subtitle, onClose, footer, children}: FormDrawerLayoutProps) {
  return (
    <Layout
      header={<DrawerHeader title={title} subtitle={subtitle} onOpenChange={() => onClose()} />}
      content={<LayoutContent>{children}</LayoutContent>}
      footer={footer && <LayoutFooter>{footer}</LayoutFooter>}
    />
  );
}

type FormActionsProps = {
  /** id of the <form> the submit button submits. */
  formId: string;
  submitLabel: string;
  isSaving: boolean;
  onCancel: () => void;
};

export function FormActions({formId, submitLabel, isSaving, onCancel}: FormActionsProps) {
  return (
    <HStack gap={2} hAlign="end">
      <Button label="Cancel" variant="secondary" onClick={onCancel} />
      <Button label={submitLabel} variant="primary" type="submit" form={formId} isLoading={isSaving} />
    </HStack>
  );
}
