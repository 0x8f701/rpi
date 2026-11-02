import { originAuthority, parseRunnerInviteLink } from '../src/protocol';
import { tlsPinHost } from '../src/tlsPin';

// React Native ships a URL whose accessors all throw
// (node_modules/react-native/Libraries/Blob/URL.js:169-232, only `href`,
// `toString` and the blob paths work). Jest runs on Node, whose URL is
// complete, so a pairing test without this shim stays green on CI while the
// device fails with `URL.protocol is not implemented`. This file swaps in a
// shim that throws exactly like the runtime one.
const UNIMPLEMENTED = ['hash', 'host', 'hostname', 'origin', 'password', 'pathname', 'port', 'protocol', 'search', 'username'];

class NativeLikeUrl {
  constructor(readonly href: string) {}
  toString(): string {
    return this.href;
  }
  toJSON(): string {
    return this.href;
  }
}

for (const prop of UNIMPLEMENTED) {
  Object.defineProperty(NativeLikeUrl.prototype, prop, {
    get() {
      throw new Error(prop === 'pathname' ? 'URL.pathname not implemented' : `URL.${prop} is not implemented`);
    },
  });
}

const realUrl = globalThis.URL;

describe('pairing without a working URL', () => {
  beforeAll(() => {
    globalThis.URL = NativeLikeUrl as unknown as typeof URL;
  });

  afterAll(() => {
    globalThis.URL = realUrl;
  });

  it('mirrors the runtime shim so the assertions cannot pass vacuously', () => {
    expect(() => new URL('https://h/chat').protocol).toThrow('URL.protocol is not implemented');
    expect(() => new URL('https://h/chat').pathname).toThrow('URL.pathname not implemented');
    expect(() => new URL('https://h/chat').host).toThrow('URL.host is not implemented');
  });

  it('parses a scanned invite link into origin, authority and ws url', () => {
    const parsed = parseRunnerInviteLink('http://198.51.100.2:8790/chat#scan-e2e-1');
    expect(parsed.endpoint).toEqual({
      origin: 'http://198.51.100.2:8790',
      authority: '198.51.100.2:8790',
      webSocketUrl: 'ws://198.51.100.2:8790/ws',
    });
    expect(parsed.token).toBe('scan-e2e-1');
  });

  it('defaults a scheme-less link to https and reads the fragment', () => {
    const parsed = parseRunnerInviteLink('192.0.2.15:8765/web#tok-123');
    expect(parsed.endpoint).toEqual({
      origin: 'https://192.0.2.15:8765',
      authority: '192.0.2.15:8765',
      webSocketUrl: 'wss://192.0.2.15:8765/ws',
    });
    expect(parsed.token).toBe('tok-123');
  });

  it('lowercases the scheme and host and drops the default port', () => {
    expect(parseRunnerInviteLink('HTTPS://H:443/chat#t').endpoint).toEqual({
      origin: 'https://h',
      authority: 'h',
      webSocketUrl: 'wss://h/ws',
    });
    expect(parseRunnerInviteLink('http://H:80/chat#t').endpoint).toEqual({
      origin: 'http://h',
      authority: 'h',
      webSocketUrl: 'ws://h/ws',
    });
    expect(parseRunnerInviteLink('https://H:8765/chat#t').endpoint.authority).toBe('h:8765');
  });

  it('keeps empty userinfo acceptable and the fragment out of the origin', () => {
    expect(parseRunnerInviteLink('https://@h/chat#x').endpoint.origin).toBe('https://h');
    expect(parseRunnerInviteLink('https://:@h/chat').endpoint.origin).toBe('https://h');
  });

  it('rejects with the documented errors', () => {
    expect(() => parseRunnerInviteLink('')).toThrow('Runner address is required.');
    expect(() => parseRunnerInviteLink('   ')).toThrow('Runner address is required.');
    expect(() => parseRunnerInviteLink('https://')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('https://not a url')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('https://h:99999/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('https://a%20b/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('//h/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('ftp://h/chat')).toThrow('Runner address must use HTTP or HTTPS.');
    expect(() => parseRunnerInviteLink('https://user:pass@h/chat')).toThrow('Runner address must not contain credentials or a query.');
    expect(() => parseRunnerInviteLink('https://user@h/chat')).toThrow('Runner address must not contain credentials or a query.');
    expect(() => parseRunnerInviteLink('https://h/chat?x=1')).toThrow('Runner address must not contain credentials or a query.');
    expect(() => parseRunnerInviteLink('https://h/other')).toThrow('Runner address must be an origin or end in /web or /chat.');
    expect(() => parseRunnerInviteLink('https://h/chat/')).toThrow('Runner address must be an origin or end in /web or /chat.');
    expect(() => parseRunnerInviteLink('https://h//chat')).toThrow('Runner address must be an origin or end in /web or /chat.');
    expect(() => parseRunnerInviteLink('https://h/chat#%zz')).toThrow('Runner invite link hash is not decodable.');
  });

  it('keeps IPv6 brackets in the urls and in the pin key', () => {
    const parsed = parseRunnerInviteLink('https://[::1]:8765/chat#t');
    expect(parsed.endpoint).toEqual({
      origin: 'https://[::1]:8765',
      authority: '[::1]:8765',
      webSocketUrl: 'wss://[::1]:8765/ws',
    });
    expect(parsed.token).toBe('t');
    // Android keys the pin by Uri.getHost(), whose reverse port scan keeps the
    // brackets (AOSP Uri.parseHost + findPortSeparator), so JS must keep them
    // too — the same string is the trust-check key on both sides.
    expect(tlsPinHost('https://[::1]:8765/chat')).toBe('[::1]');
    expect(tlsPinHost('https://[::FFFF:1]:8765')).toBe('[::ffff:1]');
    expect(originAuthority('https://[::1]:8765')).toBe('[::1]:8765');
  });

  it('refuses port 0 and non-ASCII hosts', () => {
    expect(() => parseRunnerInviteLink('https://h:0/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('http://h:0')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('https://héllo:8765/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(() => parseRunnerInviteLink('https://日本/chat')).toThrow('Runner address must be an HTTP or HTTPS origin.');
    expect(tlsPinHost('https://héllo:8765')).toBe('');
    expect(originAuthority('https://héllo:8765')).toBe('https://héllo:8765');
    expect(parseRunnerInviteLink('https://xn--hllo-bpa:8765/chat').endpoint.authority).toBe('xn--hllo-bpa:8765');
    expect(parseRunnerInviteLink('https://h:8765/chat').endpoint.authority).toBe('h:8765');
  });

  it('refuses malformed IPv6 literals', () => {
    for (const input of [
      'https://[v]/chat',
      'https://[1]/chat',
      'https://[::::]/chat',
      'https://[1:2:3:4:5:6:7:8:9]/chat',
      'https://[12345::]/chat',
      'https://[::ffff:999.1.1.1]/chat',
    ]) {
      expect(() => parseRunnerInviteLink(input)).toThrow('Runner address must be an HTTP or HTTPS origin.');
    }
  });

  it('keeps well-formed IPv6 literals and bracket controls', () => {
    expect(parseRunnerInviteLink('https://[::1]/chat').endpoint).toEqual({
      origin: 'https://[::1]',
      authority: '[::1]',
      webSocketUrl: 'wss://[::1]/ws',
    });
    expect(parseRunnerInviteLink('https://[2001:db8::1]:8765/chat').endpoint).toEqual({
      origin: 'https://[2001:db8::1]:8765',
      authority: '[2001:db8::1]:8765',
      webSocketUrl: 'wss://[2001:db8::1]:8765/ws',
    });
    // Valid, but kept as written: this path does not normalise IPv6 (Node would
    // serialise the quad as [::ffff:c000:201]).
    expect(parseRunnerInviteLink('https://[::ffff:192.0.2.1]/chat').endpoint.authority).toBe('[::ffff:192.0.2.1]');
    expect(parseRunnerInviteLink('https://[::]/chat').endpoint.authority).toBe('[::]');
    for (const input of [
      'https://[::1/chat',
      'https://host]/chat',
      'https://[::1]junk:8765/chat',
      'https://[::1]:8765junk/chat',
      'https://[]/chat',
      'https://[1:2:3:4:5:6:7:192.0.2.1]/chat',
      'https://[::1:2:3:4:5:6:7:8]/chat',
      'https://[::ffff:1.2]/chat',
      '@',
    ]) {
      expect(() => parseRunnerInviteLink(input)).toThrow('Runner address must be an HTTP or HTTPS origin.');
    }
  });

  it('counts a password that starts with a colon as credentials', () => {
    for (const input of [
      'https://::secret@host/chat',
      'https://::@host/chat',
      'https://user@host/chat',
      'https://user:pass@host/chat',
      'https://a@b@host/chat',
      'https://:pass@host/chat',
    ]) {
      expect(() => parseRunnerInviteLink(input)).toThrow('Runner address must not contain credentials or a query.');
    }
    expect(parseRunnerInviteLink('https://@host/chat').endpoint.origin).toBe('https://host');
    expect(parseRunnerInviteLink('https://:@host/chat').endpoint.origin).toBe('https://host');
  });

  it('extracts the pin host without URL', () => {
    expect(tlsPinHost('https://192.0.2.15:8765/chat')).toBe('192.0.2.15');
    expect(tlsPinHost('  https://H.Example.com:8765  ')).toBe('h.example.com');
    expect(tlsPinHost('192.0.2.15:8765')).toBe('');
    expect(tlsPinHost('')).toBe('');
  });

  it('reduces discovered origins to the authority', () => {
    expect(originAuthority('192.0.2.15:8765')).toBe('192.0.2.15:8765');
    expect(originAuthority('https://192.0.2.15:8765')).toBe('192.0.2.15:8765');
    expect(originAuthority('192.0.2.15:8765/chat')).toBe('192.0.2.15:8765');
    expect(originAuthority('ws://198.51.100.2:8790/ws')).toBe('198.51.100.2:8790');
    expect(originAuthority('http://h:80/x')).toBe('h');
    expect(originAuthority('about:blank')).toBe('about:blank');
  });
});