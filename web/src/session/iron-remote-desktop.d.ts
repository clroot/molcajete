// The web component of @devolutions/iron-remote-desktop, defined when the
// package is imported.
import type {DetailedHTMLProps, HTMLAttributes} from 'react';

declare module 'react' {
  namespace JSX {
    interface IntrinsicElements {
      'iron-remote-desktop': DetailedHTMLProps<HTMLAttributes<HTMLElement>, HTMLElement> & {
        scale?: 'fit' | 'full' | 'real';
        flexcenter?: 'true' | 'false';
        verbose?: 'true' | 'false';
        /** The protocol backend (`Backend` of @devolutions/iron-remote-desktop-rdp). */
        module?: unknown;
      };
    }
  }
}
