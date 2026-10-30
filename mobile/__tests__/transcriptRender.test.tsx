import React from 'react';
import { Clipboard, StyleSheet, Text } from 'react-native';
import renderer, { act } from 'react-test-renderer';
import { MarkdownBody } from '../src/MarkdownBody';
import { humanToolTitle, TranscriptItemView } from '../src/TranscriptItemView';
import { normalizeThinkingNewlines } from '../src/markdown';
import type { TranscriptItem } from '../src/transcriptItems';
import { DARK } from '../src/ui';

const TOOL_RESULT_LINE_LIMIT = 8;
const BASH_OUTPUT_LINE_LIMIT = 10;

let setString: jest.SpyInstance;

beforeEach(() => {
  setString = jest.spyOn(Clipboard, 'setString').mockImplementation(() => {});
});

afterEach(() => {
  setString.mockRestore();
});

const lines = (count: number) => Array.from({ length: count }, (_, index) => `L${index}`).join('\n');

const toolItem = (over: Partial<Extract<TranscriptItem, { kind: 'tool' }>>): TranscriptItem => ({
  kind: 'tool',
  id: 'tool-1',
  toolCallId: 'call-1',
  toolName: 'bash',
  args: '{"cmd":"ls"}',
  result: '',
  status: 'done',
  ...over,
});

const bashItem = (over: Partial<Extract<TranscriptItem, { kind: 'bash' }>>): TranscriptItem => ({
  kind: 'bash',
  id: 'bash-1',
  command: 'ls -la',
  output: '',
  status: 'done',
  ...over,
});

const assistantItem = (thinking: string): TranscriptItem => ({
  kind: 'assistant',
  id: 'assistant-1',
  status: 'final',
  blocks: [{ type: 'thinking', thinking }],
});

async function mount(element: React.ReactElement): Promise<renderer.ReactTestRenderer> {
  let tree!: renderer.ReactTestRenderer;
  await act(async () => {
    tree = renderer.create(element);
  });
  return tree;
}

async function unmount(tree: renderer.ReactTestRenderer): Promise<void> {
  await act(async () => {
    tree.unmount();
  });
}

async function press(tree: renderer.ReactTestRenderer, label: string): Promise<void> {
  const control = tree.root.findAll((node) => node.props.accessibilityLabel === label)[0];
  expect(control).toBeTruthy();
  await act(async () => {
    control.props.onPress();
  });
}

function textInstances(tree: renderer.ReactTestRenderer): renderer.ReactTestInstance[] {
  return tree.root.findAllByType(Text);
}

/** Every string the tree renders, collected from Text children (nested Text
 *  runs included) — the same instance surface the other RN tests read. */
function renderedStrings(tree: renderer.ReactTestRenderer): string[] {
  const out: string[] = [];
  const walk = (value: unknown): void => {
    if (typeof value === 'string') {
      out.push(value);
      return;
    }
    if (Array.isArray(value)) {
      value.forEach(walk);
      return;
    }
    if (React.isValidElement(value)) walk((value as React.ReactElement<{ children?: unknown }>).props.children);
  };
  for (const node of textInstances(tree)) walk(node.props.children);
  return out;
}

const renderedText = (tree: renderer.ReactTestRenderer) => renderedStrings(tree).join('\n');

function styleOf(tree: renderer.ReactTestRenderer, value: string): Record<string, unknown> {
  const node = textInstances(tree).find((instance) => instance.props.children === value);
  if (!node) return {};
  const own = node.props.style ? (StyleSheet.flatten(node.props.style) as Record<string, unknown>) : {};
  const parentStyle = node.parent && node.parent.props.style
    ? (StyleSheet.flatten(node.parent.props.style) as Record<string, unknown>)
    : {};
  return { ...own, ...parentStyle };
}

/** Card title = the only 15px Text the card renders (styles.cardTitle). */
function cardTitle(tree: renderer.ReactTestRenderer): string | undefined {
  for (const instance of textInstances(tree)) {
    const flat = instance.props.style ? (StyleSheet.flatten(instance.props.style) as Record<string, unknown>) : {};
    if (flat.fontSize === 15 && typeof instance.props.children === 'string') return instance.props.children;
  }
  return undefined;
}

describe('tool card', () => {
  it('humanizes known tool names and keeps the raw name for unknown ones', async () => {
    expect(humanToolTitle('bash')).toBe('Command');
    expect(humanToolTitle('web_search')).toBe('Web Search');
    expect(humanToolTitle('irc')).toBe('IRC');
    expect(humanToolTitle('weird_tool')).toBe('Weird Tool');
    expect(humanToolTitle('Frobnicate')).toBe('Frobnicate');

    const known = await mount(<TranscriptItemView item={toolItem({ toolName: 'bash' })} palette={DARK} />);
    expect(cardTitle(known)).toBe('Command');
    expect(renderedStrings(known).some((text) => text.includes('bash'))).toBe(true);
    await unmount(known);

    const unknown = await mount(<TranscriptItemView item={toolItem({ toolName: 'weird_tool' })} palette={DARK} />);
    expect(cardTitle(unknown)).toBe('Weird Tool');
    expect(renderedStrings(unknown).some((text) => text.includes('weird_tool'))).toBe(true);
    await unmount(unknown);

    const bare = await mount(<TranscriptItemView item={toolItem({ toolName: 'Frobnicate' })} palette={DARK} />);
    expect(cardTitle(bare)).toBe('Frobnicate');
    await unmount(bare);
  });

  it('bounds a long result and reveals the rest through the fold', async () => {
    const result = lines(TOOL_RESULT_LINE_LIMIT + 12);
    const tree = await mount(<TranscriptItemView item={toolItem({ result })} palette={DARK} />);
    expect(renderedText(tree)).toContain('L7');
    expect(renderedText(tree)).not.toContain('L19');

    await press(tree, 'Expand result');
    expect(renderedText(tree)).toContain('L19');
    await unmount(tree);
  });

  it('bounds raw args and a long result, and copies the whole result', async () => {
    const args = Array.from({ length: 12 }, (_, index) => `A${index}`).join('\n');
    const result = Array.from({ length: 20 }, (_, index) => `R${index}`).join('\n');
    const tree = await mount(<TranscriptItemView item={toolItem({ args, result })} palette={DARK} />);
    expect(renderedText(tree)).toContain('A5');
    expect(renderedText(tree)).not.toContain('A6');
    expect(renderedText(tree)).toContain('R7');
    expect(renderedText(tree)).not.toContain('R8');

    await press(tree, 'Copy result');
    expect(setString).toHaveBeenCalledWith(result);
    await unmount(tree);
  });

  it('copies a redacted payload', async () => {
    const tree = await mount(
      <TranscriptItemView item={toolItem({ result: 'token=sk-abcdefghijklmnopqrstuv' })} palette={DARK} />,
    );
    await press(tree, 'Copy result');
    expect(setString).toHaveBeenCalledWith('token=[REDACTED]');
    await unmount(tree);
  });
});

describe('bash card', () => {
  it('keeps the command visible and bounds the output until expanded', async () => {
    const output = lines(BASH_OUTPUT_LINE_LIMIT + 4);
    const tree = await mount(<TranscriptItemView item={bashItem({ output })} palette={DARK} />);
    expect(cardTitle(tree)).toBe('ls -la');
    expect(renderedText(tree)).toContain('L9');
    expect(renderedText(tree)).not.toContain('L13');

    await press(tree, 'Expand output');
    expect(renderedText(tree)).toContain('L13');
    await unmount(tree);
  });

  it('copies the whole output even while the card is collapsed', async () => {
    const output = lines(BASH_OUTPUT_LINE_LIMIT + 4);
    const tree = await mount(<TranscriptItemView item={bashItem({ output })} palette={DARK} />);
    await press(tree, 'Copy output');
    expect(setString).toHaveBeenCalledWith(output);
    await unmount(tree);
  });
});

describe('thinking fold', () => {
  it('renders thinking through the markdown pipeline with newline normalization', async () => {
    const tree = await mount(<TranscriptItemView item={assistantItem('# Plan\n\n**bold** step')} palette={DARK} />);
    expect(renderedText(tree)).not.toContain('Plan');

    await press(tree, 'Thinking');
    const body = renderedText(tree);
    expect(body).not.toContain('#');
    expect(body).not.toContain('**');
    expect(body).toContain('Plan');
    expect(styleOf(tree, 'bold').fontWeight).toBe('700');
    await unmount(tree);

    const escaped = await mount(<TranscriptItemView item={assistantItem('alpha\\nbeta\\ngamma')} palette={DARK} />);
    await press(escaped, 'Thinking');
    expect(renderedStrings(escaped).some((text) => text.includes('alpha\nbeta'))).toBe(true);
    await unmount(escaped);

    expect(normalizeThinkingNewlines('print("a\\nb")')).toBe('print("a\\nb")');
  });
});

describe('markdown tables', () => {
  it('renders a pipe table as a table instead of raw pipes', async () => {
    const table = ['| name | qty |', '| :--- | ---: |', '| alice | 30 |', '| bob | 4 |'].join('\n');
    const tree = await mount(<MarkdownBody text={table} palette={DARK} />);
    const body = renderedText(tree);
    expect(body).not.toContain('|');
    expect(body).not.toContain('---');
    for (const cell of ['name', 'qty', 'alice', '30', 'bob', '4']) expect(body).toContain(cell);

    expect(styleOf(tree, 'name').fontWeight).toBe('700');
    expect(styleOf(tree, 'alice').textAlign).toBe('left');
    expect(styleOf(tree, '30').textAlign).toBe('right');
    await unmount(tree);
  });
});
