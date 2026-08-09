// Shared wire shapes of the rpi control plane (see crates/pi-cli/src/modes/rpc.rs).

export interface ContentBlock {
  type: string;
  text?: string;
  thinking?: string;
  data?: string;
  mimeType?: string;
  toolCall?: { id: string; name: string; arguments: unknown };
}

export interface Model {
  id: string;
  name: string;
  provider: string;
}

export interface SessionState {
  model?: Model | null;
  thinkingLevel?: string;
  isStreaming?: boolean;
  sessionName?: string | null;
}

export interface RpcResponse {
  id?: string;
  type: string;
  command: string;
  success: boolean;
  data?: unknown;
  error?: string;
}

export interface EventFrame {
  type: string;
  [key: string]: unknown;
}

export interface AssistantMessageEvent {
  type: string;
  delta?: string;
  content?: string;
  toolCall?: unknown;
  [key: string]: unknown;
}
