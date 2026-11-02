import React from 'react';
import { Linking } from 'react-native';
import renderer, { act } from 'react-test-renderer';
import { TrustCertificate } from '../src/TrustCertificate';
import { DARK } from '../src/ui';
import type { RunnerView } from '../src/registry';
import { RunnersScreen } from '../src/screens/RunnersScreen';
import { RunnerDetailScreen } from '../src/screens/RunnerDetailScreen';

jest.mock('../src/runtime', () => ({
  registry: { client: jest.fn(), connect: jest.fn() },
}));

jest.mock('../src/screens/SessionWorkspaceScreen', () => ({
  SessionWorkspaceScreen: () => null,
}));

jest.mock('../src/tlsPin', () => {
  const actual = jest.requireActual('../src/tlsPin');
  return {
    ...actual,
    inspectRunnerTls: jest.fn(),
    trustRunnerTls: jest.fn(),
  };
});

import { inspectRunnerTls, trustRunnerTls } from '../src/tlsPin';
import { registry } from '../src/runtime';

const inspect = inspectRunnerTls as jest.MockedFunction<typeof inspectRunnerTls>;
const trust = trustRunnerTls as jest.MockedFunction<typeof trustRunnerTls>;

let tree: renderer.ReactTestRenderer | null = null;

beforeEach(() => {
  inspect.mockReset();
  trust.mockReset();
  jest.clearAllMocks();
});

afterEach(async () => {
  if (tree) {
    await act(async () => {
      tree!.unmount();
    });
    tree = null;
  }
});

function findByLabel(label: string) {
  if (!tree) throw new Error('renderer is not mounted');
  return tree.root.findAll((node) => node.props.accessibilityLabel === label)[0];
}

const preview = {
  host: '192.0.2.15',
  sha256: 'a'.repeat(64),
  subject: 'CN=psy',
  issuer: 'CN=psy',
  notAfterMs: 1,
};

it('inspects and pins a certificate in-app without opening a browser', async () => {
  const openUrl = jest.spyOn(Linking, 'openURL').mockResolvedValue(undefined as never);
  inspect.mockResolvedValue(preview);
  trust.mockResolvedValue(undefined);
  const onError = jest.fn();
  const onTrusted = jest.fn();
  await act(async () => {
    tree = renderer.create(
      <TrustCertificate
        origin="https://192.0.2.15:8765"
        palette={DARK}
        onError={onError}
        onTrusted={onTrusted}
      />,
    );
  });
  await act(async () => {
    await findByLabel('Trust certificate').props.onPress();
  });
  expect(inspect).toHaveBeenCalledWith('https://192.0.2.15:8765');
  expect(openUrl).not.toHaveBeenCalled();
  await act(async () => {
    await findByLabel('Confirm trust').props.onPress();
  });
  expect(trust).toHaveBeenCalledWith('https://192.0.2.15:8765', preview.sha256);
  expect(onTrusted).toHaveBeenCalledTimes(1);
  expect(onError).not.toHaveBeenCalled();
  expect(openUrl).not.toHaveBeenCalled();
  openUrl.mockRestore();
});

it('surfaces inspect failure through onError and never opens /chat', async () => {
  const openUrl = jest.spyOn(Linking, 'openURL').mockResolvedValue(undefined as never);
  inspect.mockRejectedValue(new Error('Peer presented no certificate.'));
  const onError = jest.fn();
  await act(async () => {
    tree = renderer.create(
      <TrustCertificate
        origin="https://192.0.2.6:8765"
        palette={DARK}
        onError={onError}
        onTrusted={jest.fn()}
      />,
    );
  });
  await act(async () => {
    await findByLabel('Trust certificate').props.onPress();
  });
  expect(onError).toHaveBeenCalledWith('Peer presented no certificate.');
  expect(openUrl).not.toHaveBeenCalled();
  expect(trust).not.toHaveBeenCalled();
  openUrl.mockRestore();
});

it.each(['list', 'detail'] as const)('%s exposes trust before a TLS error and reconnects after confirmation', async (screen) => {
  const runner: RunnerView = {
    id: 'r1', label: 'psy', origin: 'https://192.0.2.15:8765',
    authority: '192.0.2.15:8765', token: '', createdAt: 1,
    state: 'connecting', error: '', errorKind: null, probe: null,
  };
  const onError = jest.fn();
  const renderScreen = () => screen === 'list' ? (
    <RunnersScreen palette={DARK} runners={[runner]} lanCandidates={[]}
      onOpenRunner={jest.fn()} onAdd={jest.fn()} onAddLan={jest.fn()} onError={onError} />
  ) : (
    <RunnerDetailScreen palette={DARK} runnerId={runner.id} runners={[runner]}
      onBack={jest.fn()} onOpenTerminal={jest.fn()} onDeleted={jest.fn()} onError={onError} />
  );
  await act(async () => { tree = renderer.create(renderScreen()); });
  expect(findByLabel('Trust certificate')).toBeDefined();
  inspect.mockResolvedValue(preview);
  trust.mockResolvedValue(undefined);
  await act(async () => { await findByLabel('Trust certificate').props.onPress(); });
  await act(async () => { await findByLabel('Confirm trust').props.onPress(); });
  expect(trust).toHaveBeenCalledWith(runner.origin, preview.sha256);
  expect(registry.connect).toHaveBeenCalledWith(runner.id);
  expect(onError).not.toHaveBeenCalled();

  for (const state of ['reconnecting', 'offline', 'token_required', 'connected'] as const) {
    runner.state = state;
    await act(async () => { tree!.update(renderScreen()); });
    expect(Boolean(findByLabel('Trust certificate'))).toBe(state !== 'connected');
  }
  runner.state = 'connecting';
  runner.origin = 'http://192.0.2.15:8765';
  await act(async () => { tree!.update(renderScreen()); });
  expect(findByLabel('Trust certificate')).toBeUndefined();
});
