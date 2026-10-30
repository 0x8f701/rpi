import type { FileExplorerTreeTone } from '../fileExplorer';

/**
 * 16px file-type marks for the workspace tree. Strokes use currentColor so
 * selected, disabled, and git-only row colors reach the icon. No text nodes:
 * language identity is the path silhouette, not a letter badge.
 */
const ICON_PATHS: Record<FileExplorerTreeTone, readonly string[]> = {
  dir: [
    'M2 4.5h4.2l1.3 1.5H14v7H2z',
  ],
  typescript: [
    'M3 3.2h10v9.6H3z',
    'M6.2 6.4h3.6',
    'M8 6.4v4.4',
  ],
  javascript: [
    'M4 3h8.2v9.6H4z',
    'M6.4 10.4c.5.7 1.1.9 1.8.6.6-.3.6-.8.6-1.2V6.6',
  ],
  rust: [
    'M3.2 8c1.2-2.6 2.8-3.8 4.8-3.8S11.6 5.4 12.8 8c-1.2 2.6-2.8 3.8-4.8 3.8S4.4 10.6 3.2 8z',
    'M6.6 8h2.8',
    'M8 6.7v2.6',
  ],
  solidity: [
    'M8 1.6 11.2 4 8 6.4 4.8 4z',
    'M4.8 6.2 8 8.6 8 13.2 4.8 10.8z',
    'M11.2 6.2 8 8.6 8 13.2 11.2 10.8z',
  ],
  python: [
    'M6 3.2h4.4c1.2 0 1.8.7 1.8 1.8v1.6H6.6c-1.4 0-2 .7-2 1.8v.4',
    'M10 12.8H5.6c-1.2 0-1.8-.7-1.8-1.8V9.4h5.6c1.4 0 2-.7 2-1.8v-.4',
    'M7.2 4.6h.1',
    'M8.7 11.4h.1',
  ],
  go: [
    'M2.4 8.6c1.6-.9 3-.9 4.4 0 1.4.9 2.6.9 4 0 1-.7 2-.7 3.2 0',
    'M3.2 6.4c.8.5 1.6.4 2.4-.2',
    'M5.4 5.6v.1',
  ],
  markdown: [
    'M2.6 4.2h10.8v7.6H2.6z',
    'M4.4 9.6V6.4l1.6 2 1.6-2v3.2',
    'M10 6.6v3l1.4-1.5',
  ],
  json: [
    'M6.4 3.2c-1.6.6-2.2 1.6-2.2 2.6 0 1.4 1.2 1.6 1.2 2.2s-1.2.8-1.2 2.2c0 1 .6 2 2.2 2.6',
    'M9.6 3.2c1.6.6 2.2 1.6 2.2 2.6 0 1.4-1.2 1.6-1.2 2.2s1.2.8 1.2 2.2c0 1-.6 2-2.2 2.6',
  ],
  css: [
    'M5.2 3.4 4.2 12.6 8 14l3.8-1.4 1-9.2z',
    'M6.4 6h4.2',
    'M6.2 8.4h3.6',
  ],
  html: [
    'M5.4 5.2 2.8 8l2.6 2.8',
    'M10.6 5.2 13.2 8l-2.6 2.8',
    'M9 3.6 7 12.4',
  ],
  image: [
    'M2.4 3.4h11.2v9.2H2.4z',
    'M2.8 10.6 5.6 7.8l2.2 2.2 1.6-1.4 3.4 3',
    'M10.6 6.2h.1',
  ],
  config: [
    'M3 4.6h10',
    'M3 8h10',
    'M3 11.4h10',
    'M6 3.6v2',
    'M10.4 7v2',
    'M7.2 10.4v2',
  ],
  shell: [
    'M2.6 3.2h10.8v9.6H2.6z',
    'M4.6 6.4 6.8 8 4.6 9.6',
    'M8 9.8h2.8',
  ],
  document: [
    'M4 2.4h5.2L12.4 5.6V13.6H4z',
    'M9.2 2.6v3.2h3',
    'M6 8.2h4.4',
    'M6 10.6h3.2',
  ],
  file: [
    'M4.4 2.4h4.8L12 5.2v8.4H4.4z',
    'M9.2 2.6v3h2.6',
  ],
};

const DIR_OPEN_PATHS = [
  'M2 6.2V4.6h4l1.2 1.4H14',
  'M2.6 7.2 4 12.8h10.2l1.4-5.6z',
];

export function FileTypeIcon({
  tone,
  expanded,
}: {
  tone: FileExplorerTreeTone;
  expanded?: boolean;
}) {
  const paths = tone === 'dir' && expanded ? DIR_OPEN_PATHS : ICON_PATHS[tone];
  return (
    <svg
      className="file-type-icon"
      data-icon={tone}
      viewBox="0 0 16 16"
      width="16"
      height="16"
      aria-hidden="true"
      focusable="false"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {paths.map((d) => <path key={d} d={d} />)}
    </svg>
  );
}
