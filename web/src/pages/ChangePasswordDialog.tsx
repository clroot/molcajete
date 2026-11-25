import {useState, type FormEvent} from 'react';
import {Dialog, DialogHeader} from '@astryxdesign/core/Dialog';
import {Layout, LayoutContent, LayoutFooter} from '@astryxdesign/core/Layout';
import {HStack, VStack} from '@astryxdesign/core/Stack';
import {TextInput} from '@astryxdesign/core/TextInput';
import {Button} from '@astryxdesign/core/Button';
import {Banner} from '@astryxdesign/core/Banner';
import {useToast} from '@astryxdesign/core/Toast';
import {api, errorMessage} from '../api';

type Props = {
  isOpen: boolean;
  onOpenChange: (isOpen: boolean) => void;
  /** The server ended every session; sign in again. */
  onChanged: () => void;
};

export default function ChangePasswordDialog({isOpen, onOpenChange, onChanged}: Props) {
  const [current, setCurrent] = useState('');
  const [next, setNext] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState('');
  const [isSaving, setIsSaving] = useState(false);
  const showToast = useToast();

  const close = (open: boolean) => {
    if (!open) {
      setCurrent('');
      setNext('');
      setConfirm('');
      setError('');
    }
    onOpenChange(open);
  };

  const mismatch = confirm !== '' && confirm !== next;

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!current || !next || mismatch) return;
    setIsSaving(true);
    setError('');
    try {
      await api.changePassword(current, next);
      close(false);
      // Changing the password ends every session, this one included.
      showToast({body: 'Password changed. Sign in with the new password.'});
      onChanged();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <Dialog isOpen={isOpen} onOpenChange={close} purpose="form" width={440}>
      <form onSubmit={submit}>
        <Layout
          height="auto"
          header={<DialogHeader title="Change password" onOpenChange={close} />}
          content={
            <LayoutContent>
              <VStack gap={4}>
                {error && <Banner status="error" title={error} />}
                <TextInput
                  label="Current password"
                  type="password"
                  value={current}
                  onChange={setCurrent}
                  autoComplete="current-password"
                  isRequired
                />
                <TextInput
                  label="New password"
                  type="password"
                  value={next}
                  onChange={setNext}
                  autoComplete="new-password"
                  description="At least 8 characters. You will be signed out everywhere."
                  isRequired
                />
                <TextInput
                  label="Confirm new password"
                  type="password"
                  value={confirm}
                  onChange={setConfirm}
                  autoComplete="new-password"
                  status={mismatch ? {type: 'error', message: 'The passwords do not match.'} : undefined}
                  isRequired
                />
              </VStack>
            </LayoutContent>
          }
          footer={
            <LayoutFooter>
              <HStack gap={2} hAlign="end">
                <Button label="Cancel" variant="secondary" onClick={() => close(false)} />
                <Button
                  label="Change password"
                  variant="primary"
                  type="submit"
                  isLoading={isSaving}
                  isDisabled={!current || !next || mismatch}
                />
              </HStack>
            </LayoutFooter>
          }
        />
      </form>
    </Dialog>
  );
}
