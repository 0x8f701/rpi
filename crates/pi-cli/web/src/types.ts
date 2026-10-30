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
  activeTurnModel?: Model | null;
  thinkingLevel?: string;
  isStreaming?: boolean;
  sessionName?: string | null;
  queuedSteering?: string[];
  sandbox?: SandboxSettingsWire;
  mcpServers?: McpServerSettingsWire[];
}

export type RpcErrorCode =
  | 'invalid_request'
  | 'stale_catalog_target'
  | 'invalid_selection'
  | 'runtime_unavailable'
  | 'busy_capacity'
  | 'cwd_relative'
  | 'trust_denied'
  | 'cwd_invalid'
  | 'cwd_symlink'
  | 'cwd_missing'
  | 'cwd_not_directory'
  | 'cwd_changed'
  | 'file_line_too_large'
  | 'file_unavailable'
  | 'file_symlink'
  | 'directory_unavailable'
  | 'file_conversation_capacity'
  | 'file_conversation_superseded'
  | 'file_conversation_not_found'
  | 'file_conversation_binding_mismatch'
  | 'file_conversation_owner_capture_failed'
  | 'file_conversation_open_failed'
  | 'file_conversation_prompt_empty'
  | 'file_conversation_prompt_too_large'
  | 'file_conversation_selection_invalid'
  | 'file_conversation_selection_too_large'
  | 'file_conversation_selection_out_of_range'
  | 'file_conversation_read_failed'
  | 'code_review_not_open'
  | 'session_processing';

const RPC_ERROR_CODES: Readonly<Record<RpcErrorCode, true>> = {
  invalid_request: true,
  stale_catalog_target: true,
  invalid_selection: true,
  runtime_unavailable: true,
  busy_capacity: true,
  cwd_relative: true,
  trust_denied: true,
  cwd_invalid: true,
  cwd_symlink: true,
  cwd_missing: true,
  cwd_not_directory: true,
  cwd_changed: true,
  file_line_too_large: true,
  file_unavailable: true,
  file_symlink: true,
  directory_unavailable: true,
  file_conversation_capacity: true,
  file_conversation_superseded: true,
  file_conversation_not_found: true,
  file_conversation_binding_mismatch: true,
  file_conversation_owner_capture_failed: true,
  file_conversation_open_failed: true,
  file_conversation_prompt_empty: true,
  file_conversation_prompt_too_large: true,
  file_conversation_selection_invalid: true,
  file_conversation_selection_too_large: true,
  file_conversation_selection_out_of_range: true,
  file_conversation_read_failed: true,
  code_review_not_open: true,
  session_processing: true,
};

/** Parse the closed machine-readable RPC failure code set. */
export function rpcErrorCode(data: unknown): RpcErrorCode | undefined {
  if (data === null || typeof data !== 'object' || Array.isArray(data) || !('code' in data)) return undefined;
  const code = data.code;
  return typeof code === 'string' && Object.hasOwn(RPC_ERROR_CODES, code) ? code as RpcErrorCode : undefined;
}

export function isBusyCapacityError(error: unknown): boolean {
  if (error == null || typeof error !== 'object' || Array.isArray(error) || !('rpcCode' in error)) {
    return false;
  }
  return rpcErrorCode({ code: (error as { rpcCode?: unknown }).rpcCode }) === 'busy_capacity';
}

export interface RpcResponse {
  sessionId?: string;
  turnId?: string;
  id?: string;
  type: string;
  command: string;
  success: boolean;
  data?: unknown;
  error?: string;
}

export interface EventFrame {
  type: string;
  sessionId?: string;
  runtimeIncarnation?: string;
  turnId?: string;
  itemId?: string;
  sequence?: number;
  [key: string]: unknown;
}

export interface ExactRuntimeEventFrame extends EventFrame {
  sessionId: string;
  runtimeIncarnation: string;
  sequence: number;
}

export interface SwitchHandoffDataV1 {
  sessionId: string;
  runtimeIncarnation: string;
  subscriptionRequired: true;
  lifecycleResponseVersion: 1;
  cursor: number;
  notice?: string | null;
}

export interface SubscriptionCutFrame {
  type: 'subscription_snapshot' | 'session_resync';
  sessionId: string;
  runtimeIncarnation: string;
  watermark: number;
  reason: 'cold-full' | 'warm-continuity' | 'resync';
  state: unknown;
  events: ExactRuntimeEventFrame[];
  messages?: unknown[];
  activeTurnId?: string;
}

export interface SessionUnloadFrame {
  type: 'session_unloaded';
  sessionId: string;
  runtimeIncarnation: string;
  reason: 'lru' | 'closed';
}

export interface AssistantMessageEvent {
  type: string;
  delta?: string;
  content?: string;
  toolCall?: unknown;
  [key: string]: unknown;
}

export interface SandboxSettingsWire {
  mode?: 'read-only' | 'workspace-write' | 'danger-full-access';
  network?: { mode: 'none' | 'proxy-routed' | 'full-access' };
  readOnlyRoots?: string[];
  writableRoots?: Array<{ root: string; protectedDescendants?: string[] }>;
  deniedPaths?: string[];
  hostHttp?: { destinations: Array<Record<string, unknown>> };
  credentialGrants?: string[];
  credentialGrantDefinitions?: Array<Record<string, unknown>>;
  limits?: Record<string, number>;
}

export interface McpServerSettingsWire {
  name: string;
  disabled?: boolean;
  transport?: 'stdio' | 'sse';
  command?: string;
  args?: string[];
  url?: string;
  env?: Record<string, string>;
  credentialGrants?: string[];
}
