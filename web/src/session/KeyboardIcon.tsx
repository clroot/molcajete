import type {SVGProps} from 'react';

/** A keyboard, drawn like the Heroicons outline set, which has none. */
export default function KeyboardIcon(props: SVGProps<SVGSVGElement>) {
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      fill="none"
      viewBox="0 0 24 24"
      strokeWidth={1.5}
      stroke="currentColor"
      aria-hidden="true"
      {...props}>
      <rect x="2.25" y="6" width="19.5" height="12" rx="2.25" strokeLinejoin="round" />
      {/* Two rows of keys, and the space bar. */}
      <path
        strokeLinecap="round"
        d="M6 9.75h.01M9.33 9.75h.01M12.67 9.75h.01M16 9.75h.01M18 9.75h.01M6 12.75h.01M9.33 12.75h.01M12.67 12.75h.01M16 12.75h.01M18 12.75h.01M8.25 15h7.5"
      />
    </svg>
  );
}
