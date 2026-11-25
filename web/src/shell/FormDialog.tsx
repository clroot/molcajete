// The Astryx `DialogFormDialog` block: header, fields and footer actions in
// a modal; purpose="form" keeps Escape but ignores backdrop clicks once
// something was typed.
import {useId, useState, type FormEvent, type ReactNode} from 'react';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {Banner} from '@astryxdesign/core/Banner';
import {VStack} from '@astryxdesign/core/Stack';
import {errorMessage} from '../api';
import {FormActions} from './FormDrawer';

type Props = {
  isOpen: boolean;
  title: string;
  subtitle?: string;
  submitLabel: string;
  width?: number;
  onClose: () => void;
  /** Saves; a thrown error is shown in the dialog, which stays open. */
  onSubmit: () => Promise<void>;
  children: ReactNode;
};

export default function FormDialog({
  isOpen,
  title,
  subtitle,
  submitLabel,
  width = 480,
  onClose,
  onSubmit,
  children,
}: Props) {
  return (
    <Dialog isOpen={isOpen} onOpenChange={open => !open && onClose()} purpose="form" width={width}>
      {isOpen && (
        <FormDialogContent
          title={title}
          subtitle={subtitle}
          submitLabel={submitLabel}
          onClose={onClose}
          onSubmit={onSubmit}>
          {children}
        </FormDialogContent>
      )}
    </Dialog>
  );
}

type ContentProps = Omit<Props, 'isOpen' | 'width'>;

function FormDialogContent({title, subtitle, submitLabel, onClose, onSubmit, children}: ContentProps) {
  const formId = useId();
  const [isSaving, setIsSaving] = useState(false);
  const [error, setError] = useState('');

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setError('');
    setIsSaving(true);
    try {
      await onSubmit();
    } catch (err) {
      setError(errorMessage(err));
      setIsSaving(false);
    }
  };

  return (
    <Layout
      header={<DialogHeader title={title} subtitle={subtitle} onOpenChange={() => onClose()} />}
      content={
        <LayoutContent>
          <form id={formId} onSubmit={submit} noValidate>
            <VStack gap={4}>
              {error && <Banner status="error" title={error} />}
              {children}
            </VStack>
          </form>
        </LayoutContent>
      }
      footer={
        <LayoutFooter>
          <FormActions formId={formId} submitLabel={submitLabel} isSaving={isSaving} onCancel={onClose} />
        </LayoutFooter>
      }
    />
  );
}
