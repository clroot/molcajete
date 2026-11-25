import {useState} from 'react';
import {AlertDialog} from '@astryxdesign/core/AlertDialog';
import {useToast} from '@astryxdesign/core/Toast';
import {errorMessage} from '../api';

type Props<T> = {
  /** The record to delete; the dialog is open while it is set. */
  target: T | null;
  title: (target: T) => string;
  description: string;
  onDelete: (target: T) => Promise<void>;
  /** Called after a successful delete, and on cancel. */
  onClose: (deleted: boolean) => void;
};

/** Confirms a delete, shows progress, and reports failures as a toast. */
export default function ConfirmDeleteDialog<T>({target, title, description, onDelete, onClose}: Props<T>) {
  const [isDeleting, setIsDeleting] = useState(false);
  const showToast = useToast();

  const confirm = async () => {
    if (target === null) return;
    setIsDeleting(true);
    try {
      await onDelete(target);
      onClose(true);
    } catch (err) {
      showToast({body: errorMessage(err), type: 'error'});
    } finally {
      setIsDeleting(false);
    }
  };

  return (
    <AlertDialog
      isOpen={target !== null}
      onOpenChange={open => !open && onClose(false)}
      title={target === null ? '' : title(target)}
      description={description}
      actionLabel="Delete"
      isActionLoading={isDeleting}
      onAction={confirm}
    />
  );
}
