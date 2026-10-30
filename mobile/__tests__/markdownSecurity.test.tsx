import React from 'react';
import { Alert, Linking } from 'react-native';
import renderer, { act } from 'react-test-renderer';
import { WebView } from 'react-native-webview';
import { MarkdownBody } from '../src/MarkdownBody';
import { externalLinkPrompt, parseInlineMarkdown, safeMarkdownHref } from '../src/markdown';
import { mermaidViewerHtml, viewerDiagramInjection } from '../src/mermaidSvg';
import { DARK } from '../src/ui';

const MERMAID_FENCE = '```mermaid\ngraph TD; A-->B;\n```';
const CLEAN_SVG = '<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>';

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

/** Drive one native WebView event back into the component. */
async function emit(tree: renderer.ReactTestRenderer, eventProp: string, payload: unknown): Promise<void> {
  const webView = tree.root.findAllByType(WebView)[0];
  expect(webView).toBeTruthy();
  await act(async () => {
    webView.props[eventProp](payload);
  });
}

const downloadAction = (tree: renderer.ReactTestRenderer) =>
  tree.root.findAll(
    (node) => node.props.accessibilityLabel === 'Download SVG' && typeof node.props.disabled === 'boolean',
  )[0];

afterEach(() => {
  jest.restoreAllMocks();
});

describe('external links', () => {
  it('opens a link only after the confirmation names the destination', async () => {
    const alert = jest.spyOn(Alert, 'alert').mockImplementation(() => {});
    const openURL = jest.spyOn(Linking, 'openURL').mockImplementation(async () => true);
    const href = 'https://example.com:8443/a?b=1';
    const tree = await mount(<MarkdownBody text={`see [docs](${href})`} palette={DARK} />);
    expect(alert).not.toHaveBeenCalled();

    const link = tree.root.findAll(
      (node) => typeof node.props.onPress === 'function' && node.props.children === 'docs',
    )[0];
    expect(link).toBeTruthy();
    await act(async () => {
      link.props.onPress();
    });

    expect(openURL).not.toHaveBeenCalled();
    expect(alert).toHaveBeenCalledTimes(1);
    const [title, message, buttons, options] = alert.mock.calls[0] as [
      string,
      string,
      Array<{ text: string; style?: string; onPress?: () => void }>,
      { cancelable: boolean },
    ];
    expect(title).toBe('Open external link');
    expect(message).toContain('example.com:8443');
    expect(message).toContain(href);
    expect(buttons.map((button) => button.text)).toEqual(['Cancel', 'Open in browser']);
    expect(buttons[0].style).toBe('cancel');
    expect(options).toEqual({ cancelable: true });

    await act(async () => {
      buttons[1].onPress?.();
    });
    expect(openURL).toHaveBeenCalledWith(href);
    await unmount(tree);
  });

  it('drops http and non-web schemes instead of rendering a tappable link', async () => {
    expect(safeMarkdownHref('https://example.com/x')).toBe('https://example.com/x');
    expect(safeMarkdownHref('mailto:ops@example.com')).toBe('mailto:ops@example.com');
    expect(safeMarkdownHref('http://example.com/x')).toBe('');
    expect(safeMarkdownHref('intent://scan/#Intent;scheme=zxing;end')).toBe('');
    expect(safeMarkdownHref('javascript:alert(1)')).toBe('');
    expect(parseInlineMarkdown('[x](http://example.com)').some((node) => node.type === 'link')).toBe(false);
    expect(externalLinkPrompt('mailto:ops@example.com').message).toContain('Open this link outside RpiChat?');

    const tree = await mount(
      <MarkdownBody text={'[x](http://example.com) and [ok](https://example.com)'} palette={DARK} />,
    );
    const tappable = tree.root.findAll((node) => typeof node.props.onPress === 'function');
    const labels = tappable.map((node) =>
      Array.isArray(node.props.children) ? node.props.children.join('') : String(node.props.children),
    );
    expect(new Set(labels)).toEqual(new Set(['ok']));
    await unmount(tree);
  });
});

describe('mermaid viewer', () => {
  it('blocks navigation out of the viewer document and keeps file access off', async () => {
    const tree = await mount(<MarkdownBody text={MERMAID_FENCE} palette={DARK} />);
    await press(tree, 'View');
    const webView = tree.root.findAllByType(WebView)[0];
    expect(webView).toBeTruthy();
    const allowed = (url: string) => webView.props.onShouldStartLoadWithRequest({ nativeEvent: { url } });

    expect(webView.props.allowFileAccess).toBe(false);
    expect(webView.props.setSupportMultipleWindows).toBe(false);
    expect(allowed('about:blank')).toBe(true);
    expect(allowed('file:///android_asset/')).toBe(true);
    expect(allowed('file:///android_asset/mermaid.min.js')).toBe(true);
    expect(allowed('https://evil.example/')).toBe(false);
    expect(allowed('http://evil.example/')).toBe(false);
    expect(allowed('file:///etc/passwd')).toBe(false);
    expect(allowed('data:text/html,<script>alert(1)</script>')).toBe(false);
    expect(allowed('intent://scan/#Intent;scheme=zxing;end')).toBe(false);
    // The whitelist stays '*' so hostile URLs reach the callback above:
    // react-native-webview hands every URL that FAILS originWhitelist to
    // Linking.openURL without consent (WebViewShared createOnShouldStartLoadWithRequest).
    expect(webView.props.originWhitelist).toEqual(['*']);
    await unmount(tree);
  });

  it('sanitizes the diagram SVG before it can be injected or downloaded', async () => {
    expect(viewerDiagramInjection('<svg onload="alert(1)"></svg>')).toEqual({ clean: '', script: '' });
    expect(viewerDiagramInjection('<svg><a href="javascript:alert(1)"/></svg>')).toEqual({ clean: '', script: '' });
    const injection = viewerDiagramInjection(CLEAN_SVG);
    expect(injection.clean).toBe(CLEAN_SVG);
    expect(injection.script).toContain('window.rpiViewer.showDiagram(');
    expect(injection.script).toContain(JSON.stringify(CLEAN_SVG));

    // The page injects only what RN hands it: its own render path never
    // touches innerHTML, and the single innerHTML lives in showDiagram.
    const html = mermaidViewerHtml('graph TD; A-->B;');
    expect(html.slice(html.indexOf('function start()'))).not.toContain('innerHTML');
    expect(html).toContain('post({type:\'svg\', svg: svg});');
    expect(html).toContain('showDiagram: function(svg){ stage.innerHTML = String(svg); }');

    const tree = await mount(<MarkdownBody text={MERMAID_FENCE} palette={DARK} />);
    await press(tree, 'View');
    await emit(tree, 'onMessage', {
      nativeEvent: { data: JSON.stringify({ type: 'svg', svg: '<svg onload="alert(1)"></svg>' }) },
    });
    expect(downloadAction(tree).props.disabled).toBe(true);

    await emit(tree, 'onMessage', { nativeEvent: { data: JSON.stringify({ type: 'svg', svg: CLEAN_SVG }) } });
    expect(downloadAction(tree).props.disabled).toBe(false);
    await unmount(tree);
  });
});
