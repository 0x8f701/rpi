import { shouldResumeFromBackground, RUNNER_RECONNECT_INITIAL_MS } from '../src/reconnect';
import { isClosedTransportFailure, parseTransportFailure } from '../src/transportError';

describe('foreground resume', () => {
  it('reconnects immediately when the socket is not OPEN', () => {
    expect(shouldResumeFromBackground(WebSocket.OPEN)).toBe(false);
    expect(shouldResumeFromBackground(WebSocket.CLOSED)).toBe(true);
    expect(shouldResumeFromBackground(WebSocket.CLOSING)).toBe(true);
    expect(shouldResumeFromBackground(WebSocket.CONNECTING)).toBe(true);
    expect(shouldResumeFromBackground(null)).toBe(true);
    expect(RUNNER_RECONNECT_INITIAL_MS).toBe(500);
  });

  it('classifies runner-not-connected as closed transport, not cert', () => {
    expect(isClosedTransportFailure(new Error('Runner is not connected.'))).toBe(true);
    expect(isClosedTransportFailure(new Error('Runner connection closed.'))).toBe(true);
    expect(parseTransportFailure('SSL handshake failed')).toBe('cert');
    expect(isClosedTransportFailure(new Error('SSL handshake failed'))).toBe(false);
  });
});
