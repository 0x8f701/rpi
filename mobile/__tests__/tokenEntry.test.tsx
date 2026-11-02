// Runner token entry wherever a token is needed: the QR token parser behind the
// scan, the shared scan-to-fill token modal, and the token_required workspace
// surface that now offers that entry instead of dead-ending on text.
//
// Jest entry point: `cd mobile && npx jest __tests__/tokenEntry.test.tsx`.
//
// The runtime module is mocked because the workspace reads its client straight
// from the registry: `updateToken` retires the tokenless client and connects a
// new one, so the mock records the call and flips the client state the way the
// real registry does on reconnect.

import React from 'react';
import renderer, { act } from 'react-test-renderer';
import { Modal, PermissionsAndroid, Platform, Text } from 'react-native';
import { Camera } from 'react-native-camera-kit';
import { parseRunnerInviteToken } from '../src/protocol';
import { DARK } from '../src/ui';

/** Every Camera render: one render is one commit opportunity to mount the
 *  camera, which is what the permission gate has to withhold. */
const mockCameraRenders: Array<Record<string, unknown>> = [];

jest.mock('react-native-camera-kit', () => {
  const React = require('react');
  const { View } = require('react-native');
  return {
    Camera: (props: Record<string, unknown>) => {
      mockCameraRenders.push(props);
      return React.createElement(View);
    },
    CameraType: { Back: 'back' },
  };
});

const mockUpdateToken = jest.fn(async (_id: string, _token: string) => {});

const mockClient = {
  state: 'token_required',
  error: 'listen token required',
  errorKind: null,
  probe: null,
  hasOpenSocket: () => false,
  resumeFromBackground: () => {},
  subscribe: () => {},
  subscribeFrames: () => () => {},
  subscribeReconnect: () => () => {},
  command: async (command: { type: string }) => ({
    type: 'response',
    command: command.type,
    success: false,
    error: 'not connected',
  }),
};

jest.mock('../src/runtime', () => ({
  registry: {
    client: () => mockClient,
    updateToken: async (id: string, token: string) => {
      await mockUpdateToken(id, token);
      mockClient.state = 'connecting';
    },
  },
}));

import { AddRunnerScreen } from '../src/screens/AddRunnerScreen';
import { QrScannerModal } from '../src/screens/QrScannerModal';
import { SessionWorkspaceScreen } from '../src/screens/SessionWorkspaceScreen';
import { TokenEntryModal } from '../src/screens/TokenEntryModal';

const TOKEN_REQUIRED_NOTICE = 'This runner needs its listen token before the session can open.';

/** Every rendered string leaf: what a reader of the surface can actually see. */
function renderedText(tree: renderer.ReactTestRenderer): string[] {
  return tree.root
    .findAllByType(Text)
    .flatMap((node) => (typeof node.props.children === 'string' ? [node.props.children] : []));
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: label }).props.onPress();
  });
}

async function typeInto(tree: renderer.ReactTestRenderer, label: string, value: string): Promise<void> {
  await act(async () => {
    tree.root.findByProps({ accessibilityLabel: label }).props.onChangeText(value);
  });
}

/** Deliver one barcode read to the mounted camera surface. */
async function scan(tree: renderer.ReactTestRenderer, raw: string): Promise<void> {
  const cameras = tree.root.findAllByType(Camera);
  expect(cameras).toHaveLength(1);
  await act(async () => {
    cameras[0].props.onReadCode({ nativeEvent: { codeStringValue: raw } });
  });
}

async function unmount(tree: renderer.ReactTestRenderer): Promise<void> {
  await act(async () => {
    tree.unmount();
  });
}

describe('parseRunnerInviteToken', () => {
  it('reads the token out of a host pairing link', () => {
    expect(parseRunnerInviteToken('https://192.0.2.15:8765/chat#tok-123')).toBe('tok-123');
  });

  it('percent-decodes and trims the fragment', () => {
    expect(parseRunnerInviteToken('https://192.0.2.15:8765/web#pass%40word%20one')).toBe('pass@word one');
    expect(parseRunnerInviteToken('  https://192.0.2.15:8765/chat#tok-123  ')).toBe('tok-123');
  });

  it('has no token for a link without a fragment', () => {
    expect(parseRunnerInviteToken('https://192.0.2.15:8765/chat')).toBe('');
  });

  it('keeps a bare token instead of throwing', () => {
    expect(parseRunnerInviteToken('tok-123')).toBe('tok-123');
  });

  it('trims a bare token', () => {
    expect(parseRunnerInviteToken('  tok-123\n')).toBe('tok-123');
  });

  it('is empty for a blank scan', () => {
    expect(parseRunnerInviteToken('')).toBe('');
    expect(parseRunnerInviteToken('   ')).toBe('');
  });
});

describe('TokenEntryModal', () => {
  it('fills the token field from a scanned invite link and saves the decoded token', async () => {
    const saved: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <TokenEntryModal
          visible
          palette={DARK}
          onSave={(token) => saved.push(token)}
          onCancel={() => {}}
        />,
      );
    });

    await press(tree, 'Scan QR');
    await scan(tree, 'https://192.0.2.15:8765/chat#pass%40word%20one');
    expect(tree.root.findByProps({ accessibilityLabel: 'Token' }).props.value).toBe('pass@word one');

    await press(tree, 'Save');
    expect(saved).toEqual(['pass@word one']);
    await unmount(tree);
  });

  it('saves a manually typed token', async () => {
    const saved: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <TokenEntryModal
          visible
          palette={DARK}
          onSave={(token) => saved.push(token)}
          onCancel={() => {}}
        />,
      );
    });

    await typeInto(tree, 'Token', '  tok-manual  ');
    await press(tree, 'Save');
    expect(saved).toEqual(['tok-manual']);
    await unmount(tree);
  });

  it('keeps Save gated until the field holds a token', async () => {
    const saved: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <TokenEntryModal
          visible
          palette={DARK}
          onSave={(token) => saved.push(token)}
          onCancel={() => {}}
        />,
      );
    });

    // An empty field and a whitespace-only field both leave the control
    // disabled, which is what stops the touch responder from saving at all.
    expect(tree.root.findByProps({ accessibilityLabel: 'Save' }).props.disabled).toBe(true);
    await typeInto(tree, 'Token', '   ');
    expect(tree.root.findByProps({ accessibilityLabel: 'Save' }).props.disabled).toBe(true);
    expect(saved).toEqual([]);

    await typeInto(tree, 'Token', 'tok-1');
    expect(tree.root.findByProps({ accessibilityLabel: 'Save' }).props.disabled).toBe(false);
    await press(tree, 'Save');
    expect(saved).toEqual(['tok-1']);
    await unmount(tree);
  });

  it('saves nothing for a scan that carries no token', async () => {
    const saved: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <TokenEntryModal
          visible
          palette={DARK}
          onSave={(token) => saved.push(token)}
          onCancel={() => {}}
        />,
      );
    });

    await press(tree, 'Scan QR');
    await scan(tree, 'https://192.0.2.15:8765/chat');
    expect(tree.root.findByProps({ accessibilityLabel: 'Token' }).props.value).toBe('');
    expect(tree.root.findByProps({ accessibilityLabel: 'Save' }).props.disabled).toBe(true);
    expect(saved).toEqual([]);
    await unmount(tree);
  });

  it('renders the app error toast inside the token modal', async () => {
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <TokenEntryModal
          visible
          palette={DARK}
          onSave={() => {}}
          onCancel={() => {}}
          overlayToast={<Text>toast-marker</Text>}
        />,
      );
    });

    const shown = tree.root.findAllByType(Modal).filter((node) => node.props.visible === true);
    expect(shown.length).toBeGreaterThan(0);
    expect(shown.every((node) => node.findAllByType(Text).some((child) => child.props.children === 'toast-marker'))).toBe(true);
    await unmount(tree);
  });
});

describe('QrScannerModal', () => {
  beforeEach(() => {
    mockCameraRenders.length = 0;
  });

  it('keeps the camera unmounted while the Android permission is unresolved', async () => {
    const os = Platform.OS;
    const permission = Promise.withResolvers<string>();
    const request = jest
      .spyOn(PermissionsAndroid, 'request')
      .mockImplementation(() => permission.promise);
    Object.defineProperty(Platform, 'OS', { value: 'android', configurable: true });
    let tree!: renderer.ReactTestRenderer;
    try {
      await act(async () => {
        tree = renderer.create(
          <QrScannerModal visible palette={DARK} onScanned={() => {}} onClose={() => {}} />,
        );
      });
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);

      await act(async () => {
        permission.resolve(PermissionsAndroid.RESULTS.GRANTED);
      });
      expect(tree.root.findAllByType(Camera)).toHaveLength(1);
      expect(renderedText(tree)).not.toContain('Camera permission is required to scan.');
    } finally {
      await unmount(tree);
      request.mockRestore();
      Object.defineProperty(Platform, 'OS', { value: os, configurable: true });
    }
  });

  it('never renders the camera on the first commit of a reopen, before its permission resolves', async () => {
    const os = Platform.OS;
    const pending: Array<{ resolve: (value: string) => void; reject: (error: Error) => void }> = [];
    const request = jest.spyOn(PermissionsAndroid, 'request').mockImplementation(() => {
      const deferred = Promise.withResolvers<string>();
      pending.push({ resolve: deferred.resolve, reject: deferred.reject });
      return deferred.promise;
    });
    Object.defineProperty(Platform, 'OS', { value: 'android', configurable: true });
    const scanner = (visible: boolean) => (
      <QrScannerModal visible={visible} palette={DARK} onScanned={() => {}} onClose={() => {}} />
    );
    let tree!: renderer.ReactTestRenderer;
    try {
      await act(async () => {
        tree = renderer.create(scanner(true));
      });
      await act(async () => {
        pending[0].resolve(PermissionsAndroid.RESULTS.GRANTED);
      });
      expect(mockCameraRenders).toHaveLength(1);

      mockCameraRenders.length = 0;
      await act(async () => {
        tree.update(scanner(false));
      });
      expect(mockCameraRenders).toHaveLength(0);

      // The reopen commits without a single Camera render: the previous open's
      // grant never reaches this open's first render, not even for one commit.
      await act(async () => {
        tree.update(scanner(true));
      });
      expect(mockCameraRenders).toHaveLength(0);
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');

      // This open really did ask again, and only its own grant mounts.
      expect(pending).toHaveLength(2);
      await act(async () => {
        pending[1].resolve(PermissionsAndroid.RESULTS.GRANTED);
      });
      expect(mockCameraRenders).toHaveLength(1);
    } finally {
      await unmount(tree);
      request.mockRestore();
      Object.defineProperty(Platform, 'OS', { value: os, configurable: true });
    }
  });

  it('re-asks the permission on every open instead of reusing the last grant', async () => {
    const os = Platform.OS;
    const pending: Array<{ resolve: (value: string) => void; reject: (error: Error) => void }> = [];
    const request = jest.spyOn(PermissionsAndroid, 'request').mockImplementation(() => {
      const deferred = Promise.withResolvers<string>();
      pending.push({ resolve: deferred.resolve, reject: deferred.reject });
      return deferred.promise;
    });
    Object.defineProperty(Platform, 'OS', { value: 'android', configurable: true });
    const scanner = (visible: boolean) => (
      <QrScannerModal visible={visible} palette={DARK} onScanned={() => {}} onClose={() => {}} />
    );
    let tree!: renderer.ReactTestRenderer;
    try {
      await act(async () => {
        tree = renderer.create(scanner(true));
      });
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);
      await act(async () => {
        pending[0].resolve(PermissionsAndroid.RESULTS.GRANTED);
      });
      expect(tree.root.findAllByType(Camera)).toHaveLength(1);

      // Reopen: the grant from the previous open does not carry over, so the
      // camera waits for this open's request.
      await act(async () => {
        tree.update(scanner(false));
      });
      await act(async () => {
        tree.update(scanner(true));
      });
      expect(pending).toHaveLength(2);
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');

      // This open's request rejects: still no camera, and the notice stays.
      await act(async () => {
        pending[1].reject(new Error('permission bridge failed'));
      });
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');

      // A later open that is granted brings the camera back.
      await act(async () => {
        tree.update(scanner(false));
      });
      await act(async () => {
        tree.update(scanner(true));
      });
      await act(async () => {
        pending[2].resolve(PermissionsAndroid.RESULTS.GRANTED);
      });
      expect(tree.root.findAllByType(Camera)).toHaveLength(1);
      expect(renderedText(tree)).not.toContain('Camera permission is required to scan.');
    } finally {
      await unmount(tree);
      request.mockRestore();
      Object.defineProperty(Platform, 'OS', { value: os, configurable: true });
    }
  });

  it('treats a rejected permission request as denied', async () => {
    const os = Platform.OS;
    const request = jest
      .spyOn(PermissionsAndroid, 'request')
      .mockImplementation(() => Promise.reject(new Error('permission bridge failed')));
    Object.defineProperty(Platform, 'OS', { value: 'android', configurable: true });
    let tree!: renderer.ReactTestRenderer;
    try {
      await act(async () => {
        tree = renderer.create(
          <QrScannerModal visible palette={DARK} onScanned={() => {}} onClose={() => {}} />,
        );
      });
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);
    } finally {
      await unmount(tree);
      request.mockRestore();
      Object.defineProperty(Platform, 'OS', { value: os, configurable: true });
    }
  });

  it('shows the permission message instead of a black screen when the camera is denied', async () => {
    const os = Platform.OS;
    const request = jest
      .spyOn(PermissionsAndroid, 'request')
      .mockResolvedValue(PermissionsAndroid.RESULTS.DENIED);
    Object.defineProperty(Platform, 'OS', { value: 'android', configurable: true });
    let tree!: renderer.ReactTestRenderer;
    try {
      await act(async () => {
        tree = renderer.create(
          <QrScannerModal visible palette={DARK} onScanned={() => {}} onClose={() => {}} />,
        );
      });
      expect(renderedText(tree)).toContain('Camera permission is required to scan.');
      expect(tree.root.findAllByType(Camera)).toHaveLength(0);
    } finally {
      await unmount(tree);
      request.mockRestore();
      Object.defineProperty(Platform, 'OS', { value: os, configurable: true });
    }
  });

  it('delivers one read per open even when the same code is seen twice', async () => {
    const seen: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <QrScannerModal
          visible
          palette={DARK}
          onScanned={(value) => seen.push(value)}
          onClose={() => {}}
        />,
      );
    });

    await scan(tree, 'https://host:8765/chat#tok-one');
    await scan(tree, 'https://host:8765/chat#tok-one');
    expect(seen).toEqual(['https://host:8765/chat#tok-one']);

    // The guard is per open: closing and reopening accepts the next code.
    await act(async () => {
      tree.update(
        <QrScannerModal
          visible={false}
          palette={DARK}
          onScanned={(value) => seen.push(value)}
          onClose={() => {}}
        />,
      );
    });
    await act(async () => {
      tree.update(
        <QrScannerModal
          visible
          palette={DARK}
          onScanned={(value) => seen.push(value)}
          onClose={() => {}}
        />,
      );
    });
    await scan(tree, 'https://host:8765/chat#tok-two');
    expect(seen).toEqual(['https://host:8765/chat#tok-one', 'https://host:8765/chat#tok-two']);
    await unmount(tree);
  });
});

describe('AddRunnerScreen', () => {
  it('submits the scanned link and fills the invite field', async () => {
    const added: string[] = [];
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <AddRunnerScreen
          palette={DARK}
          onAdd={async (raw) => {
            added.push(raw);
            return true;
          }}
          onClose={() => {}}
        />,
      );
    });

    expect(renderedText(tree)).toContain('Scan server QR');
    await press(tree, 'Scan QR');
    await scan(tree, 'https://192.0.2.15:8765/chat#tok-9');

    expect(added).toEqual(['https://192.0.2.15:8765/chat#tok-9']);
    expect(tree.root.findByProps({ placeholder: 'https://host:8765/chat#token' }).props.value)
      .toBe('https://192.0.2.15:8765/chat#tok-9');
    await unmount(tree);
  });
});

describe('SessionWorkspaceScreen on token_required', () => {
  beforeEach(() => {
    mockUpdateToken.mockClear();
    mockClient.state = 'token_required';
  });

  async function mountWorkspace(): Promise<renderer.ReactTestRenderer> {
    let tree!: renderer.ReactTestRenderer;
    await act(async () => {
      tree = renderer.create(
        <SessionWorkspaceScreen palette={DARK} runnerId="r1" onError={() => {}} />,
      );
    });
    return tree;
  }

  it('offers the token action instead of only the dead-end text, and saves a typed token', async () => {
    const tree = await mountWorkspace();
    expect(renderedText(tree)).toContain(TOKEN_REQUIRED_NOTICE);

    await press(tree, 'Set token');
    expect(tree.root.findByProps({ accessibilityLabel: 'Scan QR' })).toBeTruthy();
    await typeInto(tree, 'Token', 'tok-typed');
    await press(tree, 'Save');

    expect(mockUpdateToken).toHaveBeenCalledWith('r1', 'tok-typed');
    // The card is gone because the workspace rendered its session surface
    // again, not because the tree emptied out.
    expect(renderedText(tree)).toContain('No messages.');
    expect(renderedText(tree)).not.toContain(TOKEN_REQUIRED_NOTICE);
    await unmount(tree);
  });

  it('saves a scanned invite link through the same runner token update', async () => {
    const tree = await mountWorkspace();
    await press(tree, 'Set token');
    await press(tree, 'Scan QR');
    await scan(tree, 'https://192.0.2.15:8765/chat#pass%40word');
    await press(tree, 'Save');

    expect(mockUpdateToken).toHaveBeenCalledWith('r1', 'pass@word');
    expect(renderedText(tree)).toContain('No messages.');
    expect(renderedText(tree)).not.toContain(TOKEN_REQUIRED_NOTICE);
    await unmount(tree);
  });
});
