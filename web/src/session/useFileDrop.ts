import {useEffect, useRef, useState} from 'react';

const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes('Files') ?? false;

/**
 * Takes files dragged onto the page while `isEnabled`. Returns whether
 * files are being dragged over it.
 */
export function useFileDrop(isEnabled: boolean, onFiles: (files: File[]) => void): boolean {
  const [isDragging, setIsDragging] = useState(false);
  const onFilesRef = useRef(onFiles);
  useEffect(() => {
    onFilesRef.current = onFiles;
  }, [onFiles]);

  useEffect(() => {
    if (!isEnabled) return;
    // dragenter and dragleave fire for every element crossed; count them.
    let depth = 0;
    const enter = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth++;
      setIsDragging(true);
    };
    const over = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      // Without this the browser opens the file instead of dropping it here.
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
    };
    const leave = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      depth = Math.max(0, depth - 1);
      if (depth === 0) setIsDragging(false);
    };
    const drop = (e: DragEvent) => {
      if (!hasFiles(e)) return;
      e.preventDefault();
      depth = 0;
      setIsDragging(false);
      const files = [...(e.dataTransfer?.files ?? [])];
      if (files.length) onFilesRef.current(files);
    };
    window.addEventListener('dragenter', enter);
    window.addEventListener('dragover', over);
    window.addEventListener('dragleave', leave);
    window.addEventListener('drop', drop);
    return () => {
      window.removeEventListener('dragenter', enter);
      window.removeEventListener('dragover', over);
      window.removeEventListener('dragleave', leave);
      window.removeEventListener('drop', drop);
      setIsDragging(false);
    };
  }, [isEnabled]);

  return isEnabled && isDragging;
}
