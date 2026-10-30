import React from 'react';
import { Text, TextInput } from 'react-native';
import renderer, { act } from 'react-test-renderer';

const mockClient = {
  state: 'connected',
  probe: null,
  command: jest.fn(async () => ({ success: false, data: null, error: 'unavailable' })),
  subscribe: jest.fn(),
  subscribeFrames: jest.fn(() => () => {}),
  subscribeReconnect: jest.fn(() => () => {}),
  hasOpenSocket: () => true,
  resumeFromBackground: jest.fn(),
};

const mockRunner = {
  id: 'r1',
  label: 'runner',
  origin: 'http://127.0.0.1:8787',
  authority: '127.0.0.1:8787',
  token: 'tok',
  codename: null,
  createdAt: 0,
  state: 'connected',
  error: '',
  errorKind: null,
  probe: null,
};

jest.mock('../src/runtime', () => ({
  registry: {
    client: () => mockClient,
    snapshot: () => [mockRunner],
  },
  mappingStore: {},
  lanDiscovery: {},
}));

let mockWebViewProps: { onMessage: (event: { nativeEvent: { data: string } }) => void } | null = null;

jest.mock('react-native-webview', () => {
  const ReactModule = require('react');
  const { View } = require('react-native');
  return {
    WebView: ReactModule.forwardRef((props: Record<string, unknown>, ref: unknown) => {
      ReactModule.useImperativeHandle(ref, () => ({ injectJavaScript: () => {} }));
      mockWebViewProps = props as unknown as { onMessage: (event: { nativeEvent: { data: string } }) => void };
      return ReactModule.createElement(View, { testID: 'picker' });
    }),
  };
});

import { SessionWorkspaceScreen } from '../src/screens/SessionWorkspaceScreen';
import { rememberTmuxFileDestination, type VideoFrame } from '../src/attachments';
import { DARK } from '../src/ui';

const fetchMock = jest.fn();
const mockSetNativeProps = jest.fn();
// The composer clears a swallowed paste on the native input (a controlled
// TextInput does not re-push an unchanged value).
(TextInput.prototype as unknown as { setNativeProps: typeof mockSetNativeProps }).setNativeProps = mockSetNativeProps;

function respond(status: number, body: unknown) {
  return { status, ok: status >= 200 && status < 300, json: async () => body };
}

function frame(index: number, timestampSeconds: number): VideoFrame {
  return {
    index,
    timestampSeconds,
    mimeType: 'image/jpeg',
    data: Buffer.alloc(16, index + 1).toString('base64'),
  };
}

const videoBody = {
  instruction: 'Video attachment: clip.mp4 — 2 chronological frames.',
  durationSeconds: 1.5,
  mimeType: 'video/mp4',
  sizeBytes: 4096,
  frames: [frame(0, 0), frame(1, 1.5)],
};

async function renderComposer() {
  const onError = jest.fn();
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(<SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={onError} />);
  });
  const post = async (payload: unknown) => {
    await act(async () => {
      mockWebViewProps?.onMessage({ nativeEvent: { data: JSON.stringify(payload) } });
    });
  };
  return { tree, onError, post };
}

function labels(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .map((node) => node.props.children)
    .filter((value): value is string => typeof value === 'string');
}

beforeEach(() => {
  fetchMock.mockReset();
  mockSetNativeProps.mockReset();
  (global as { fetch: unknown }).fetch = fetchMock;
  rememberTmuxFileDestination(null);
});

it('uploads a picked video into a ready chip carrying its frames', async () => {
  const inFlight = Promise.withResolvers<unknown>();
  fetchMock.mockReturnValueOnce(inFlight.promise);
  const { tree, onError, post } = await renderComposer();

  await post({ type: 'begin', name: 'clip.mp4', mimeType: 'video/mp4', size: 4096, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: Buffer.alloc(4096, 3).toString('base64') });
  await post({ type: 'end' });

  // The chip is in the queue in its uploading state while the POST is in
  // flight, so the user never sees a silent gap.
  expect(labels(tree)).toContain('clip.mp4');
  expect(labels(tree).some((label) => label.includes('uploading…'))).toBe(true);
  expect(fetchMock).toHaveBeenCalledTimes(1);

  await act(async () => {
    inFlight.resolve(respond(200, videoBody));
  });
  expect(onError).not.toHaveBeenCalled();
  const [url, init] = fetchMock.mock.calls[0] as [string, { method: string; headers: Record<string, string>; body: Uint8Array }];
  expect(url).toBe('http://127.0.0.1:8787/upload/video');
  expect(init.method).toBe('POST');
  expect(init.headers['X-Video-Name']).toBe('clip.mp4');
  expect(init.headers.Authorization).toBe('Bearer tok');
  expect(init.body.length).toBe(4096);

  expect(labels(tree).some((label) => label.includes('2 frames') && label.includes('1.5 s'))).toBe(true);
  expect(labels(tree).some((label) => label.includes('uploading…'))).toBe(false);

  await act(async () => {
    tree.unmount();
  });
});

it('keeps a failed upload as an error chip whose Retry re-uploads the same bytes', async () => {
  fetchMock
    .mockResolvedValueOnce(respond(503, { error: 'video analysis is unavailable: ffmpeg is not installed on the runner' }))
    .mockResolvedValueOnce(respond(200, videoBody));
  const { tree, onError, post } = await renderComposer();

  await post({ type: 'begin', name: 'clip.mp4', mimeType: 'video/mp4', size: 4096, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: Buffer.alloc(4096, 3).toString('base64') });
  await post({ type: 'end' });
  await act(async () => {});

  expect(onError).toHaveBeenCalledWith('clip.mp4 failed to upload: video analysis is unavailable: ffmpeg is not installed on the runner');
  expect(labels(tree)).toContain('clip.mp4');
  expect(labels(tree).some((label) => label.includes('ffmpeg is not installed'))).toBe(true);

  // The failure blocks the send with an actionable message instead of dropping
  // the chip silently.
  const send = tree.root.findByProps({ accessibilityLabel: 'Send' });
  await act(async () => {
    send.props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('clip.mp4 failed to upload: video analysis is unavailable: ffmpeg is not installed on the runner — retry or remove it.');

  const retry = tree.root.findByProps({ accessibilityLabel: 'Retry clip.mp4' });
  await act(async () => {
    retry.props.onPress();
  });
  await act(async () => {});
  expect(fetchMock).toHaveBeenCalledTimes(2);
  expect(labels(tree).some((label) => label.includes('2 frames'))).toBe(true);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Retry clip.mp4' })).toHaveLength(0);

  await act(async () => {
    tree.unmount();
  });
});

it('re-sends a chip the batch budget rejected instead of stranding it in uploading', async () => {
  fetchMock.mockResolvedValue(respond(200, {
    text: 'y'.repeat(1_500_000),
    instruction: 'PDF attachment: report.pdf — extracted text layer.',
  }));
  const { tree, onError, post } = await renderComposer();

  // A 2 MiB code chip already spends most of the one-prompt-frame budget.
  await post({ type: 'begin', name: 'notes.txt', mimeType: 'text/plain', size: 2 * 1024 * 1024, encoding: 'text', total: 1 });
  await post({ type: 'chunk', data: 'x'.repeat(2 * 1024 * 1024) });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('notes.txt');

  const pdfBytes = Buffer.from('%PDF-1.7 scanned body', 'utf8');
  await post({ type: 'begin', name: 'report.pdf', mimeType: 'application/pdf', size: pdfBytes.length, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: pdfBytes.toString('base64') });
  await post({ type: 'end' });
  await act(async () => {});

  // The extracted text cannot fit beside the code chip: the chip settles on the
  // bounded reason with its bytes intact.
  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(onError).toHaveBeenCalledWith('report.pdf failed to upload: extracted text exceeds the total send size limit');
  expect(labels(tree).some((label) => label.includes('exceeds the total send size limit'))).toBe(true);
  expect(labels(tree).some((label) => label.includes('uploading…'))).toBe(false);

  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Retry report.pdf' }).props.onPress();
  });
  await act(async () => {});

  // Retry re-POSTs the SAME bytes and settles on the same bounded reason —
  // never a chip stuck in `uploading` with nothing left to send.
  expect(fetchMock).toHaveBeenCalledTimes(2);
  const [firstUrl, firstInit] = fetchMock.mock.calls[0] as [string, { body: Uint8Array }];
  const [secondUrl, secondInit] = fetchMock.mock.calls[1] as [string, { body: Uint8Array }];
  expect(firstUrl).toBe('http://127.0.0.1:8787/upload/pdf');
  expect(secondUrl).toBe('http://127.0.0.1:8787/upload/pdf');
  expect(Array.from(secondInit.body)).toEqual(Array.from(firstInit.body));
  expect(labels(tree).some((label) => label.includes('exceeds the total send size limit'))).toBe(true);
  expect(labels(tree).some((label) => label.includes('uploading…'))).toBe(false);
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Retry report.pdf' }).length).toBeGreaterThan(0);

  await act(async () => {
    tree.unmount();
  });
});

it('groups a whole selection of refused files into one summary naming them', async () => {
  const { tree, onError, post } = await renderComposer();

  await post({ type: 'reject', name: 'dump.bin', size: 12, reason: 'unsupported' });
  await post({ type: 'begin', name: 'photo.png', mimeType: 'image/png', size: 20 * 1024 * 1024 + 1, encoding: 'base64', total: 1 });
  await post({ type: 'chunk', data: 'AAAA' });
  await post({ type: 'end' });
  await post({ type: 'reject', name: 'huge.png', size: 20 * 1024 * 1024 + 1, reason: 'image-oversize' });
  expect(onError).not.toHaveBeenCalled();

  await post({ type: 'batch-end' });
  expect(onError).toHaveBeenCalledTimes(1);
  const summary = onError.mock.calls[0][0] as string;
  expect(summary).toContain('Skipped 3 file(s)');
  expect(summary).toContain('dump.bin');
  expect(summary).toContain('photo.png');
  expect(summary).toContain('huge.png');
  expect(labels(tree).some((label) => label.includes('photo.png'))).toBe(false);

  await act(async () => {
    tree.unmount();
  });
});

it('turns a large paste into a chip and keeps the typed draft', async () => {
  const { tree, onError } = await renderComposer();
  const input = tree.root.findByProps({ placeholder: 'Message' });
  const body = Array.from({ length: 40 }, (_, index) => `line ${index}`).join('\n');

  await act(async () => {
    input.props.onChangeText('draft');
  });
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('draft');

  // The pasted block leaves the input and becomes a chip; the typed text stays.
  await act(async () => {
    input.props.onChangeText(`draft${body}`);
  });
  expect(onError).not.toHaveBeenCalled();
  expect(labels(tree)).toContain('pasted-text.txt');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('draft');
  // The pasted block is cleared from the input itself, not only from state.
  expect(mockSetNativeProps).toHaveBeenCalledWith({ text: 'draft' });

  await act(async () => {
    input.props.onChangeText('draft!');
  });
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('draft!');
  expect(labels(tree)).toContain('pasted-text.txt');

  // A pasted block over the code cap is refused with a visible reason and
  // never lands in the input.
  await act(async () => {
    input.props.onChangeText(`draft!${'x'.repeat(3 * 1024 * 1024)}`);
  });
  expect(onError).toHaveBeenCalledWith('pasted-text.txt is over the 2.0 MiB per-file attachment limit.');
  expect(tree.root.findByProps({ placeholder: 'Message' }).props.value).toBe('draft!');

  await act(async () => {
    tree.unmount();
  });
});

it('applies a finished upload to the live queue, not a pre-await snapshot', async () => {
  const inFlight = Promise.withResolvers<unknown>();
  fetchMock.mockReturnValueOnce(inFlight.promise);
  const { tree, onError, post } = await renderComposer();

  await post({ type: 'begin', name: 'clip.mp4', mimeType: 'video/mp4', size: 4096, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: Buffer.alloc(4096, 3).toString('base64') });
  await post({ type: 'end' });
  expect(labels(tree).some((label) => label.includes('uploading…'))).toBe(true);

  // A second intake lands while the video upload is still in flight.
  const code = Buffer.from('const answer = 42;', 'utf8');
  await post({ type: 'begin', name: 'notes.ts', mimeType: 'text/typescript', size: code.length, encoding: 'text', total: 1 });
  await post({ type: 'chunk', data: code.toString('utf8') });
  await post({ type: 'end' });
  expect(labels(tree)).toContain('notes.ts');

  await act(async () => {
    inFlight.resolve(respond(200, videoBody));
  });
  expect(onError).not.toHaveBeenCalled();
  // The concurrent chip survives the settlement of the other upload.
  expect(labels(tree)).toContain('notes.ts');
  expect(labels(tree)).toContain('clip.mp4');
  expect(labels(tree).some((label) => label.includes('2 frames'))).toBe(true);

  await act(async () => {
    tree.unmount();
  });
});

it('does not resurrect a chip removed while its upload was in flight', async () => {
  const inFlight = Promise.withResolvers<unknown>();
  fetchMock.mockReturnValueOnce(inFlight.promise);
  const { tree, onError, post } = await renderComposer();

  await post({ type: 'begin', name: 'clip.mp4', mimeType: 'video/mp4', size: 4096, encoding: 'bytes', total: 1 });
  await post({ type: 'chunk', data: Buffer.alloc(4096, 3).toString('base64') });
  await post({ type: 'end' });
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: 'Remove clip.mp4' }).props.onPress();
  });
  expect(labels(tree)).not.toContain('clip.mp4');

  await act(async () => {
    inFlight.resolve(respond(200, videoBody));
  });
  expect(labels(tree)).not.toContain('clip.mp4');
  expect(tree.root.findAllByProps({ accessibilityLabel: 'Remove clip.mp4' })).toHaveLength(0);
  // The removed chip's in-flight outcome is stale: it must not toast a phantom.
  expect(onError).not.toHaveBeenCalled();

  await act(async () => {
    tree.unmount();
  });
});

it('sends a chip to the terminal only when a tmux destination is live', async () => {
  const png = Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), Buffer.alloc(24)]);
  const { tree, onError, post } = await renderComposer();
  await post({ type: 'begin', name: 'screenshot.png', mimeType: 'image/png', size: png.length, encoding: 'base64', total: 1 });
  await post({ type: 'chunk', data: png.toString('base64') });
  await post({ type: 'end' });

  const control = tree.root.findByProps({ accessibilityLabel: 'Send screenshot.png to terminal' });
  await act(async () => {
    control.props.onPress();
  });
  await act(async () => {});
  expect(fetchMock).not.toHaveBeenCalled();
  expect(onError).toHaveBeenCalledWith('Open a tmux terminal first');

  rememberTmuxFileDestination({ runnerId: 'r1', session: 'work', origin: 'http://127.0.0.1:8787', token: 'tok' });
  fetchMock.mockResolvedValueOnce(respond(200, { path: '/tmp/rpi-tmux-file-1/screenshot.png' }));
  await act(async () => {
    control.props.onPress();
  });
  await act(async () => {});
  expect(fetchMock).toHaveBeenCalledTimes(1);
  const [url, init] = fetchMock.mock.calls[0] as [string, { method: string; headers: Record<string, string>; body: Uint8Array }];
  expect(url).toBe('http://127.0.0.1:8787/upload/tmux-file');
  expect(init.method).toBe('POST');
  expect(init.headers['X-File-Name']).toBe('screenshot.png');
  expect(init.headers['X-Tmux-Session']).toBe('work');
  expect(init.headers.Authorization).toBe('Bearer tok');
  expect(init.body.length).toBe(png.length);
  expect(onError).toHaveBeenCalledTimes(1);

  // A failed terminal send surfaces the backend reason instead of a silent drop.
  fetchMock.mockResolvedValueOnce(respond(413, { error: 'upload exceeds the 8 MiB tmux-file limit' }));
  await act(async () => {
    control.props.onPress();
  });
  await act(async () => {});
  expect(onError).toHaveBeenLastCalledWith('upload exceeds the 8 MiB tmux-file limit');

  await act(async () => {
    tree.unmount();
  });
});
