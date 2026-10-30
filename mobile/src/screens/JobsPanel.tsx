import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ScrollView, Text, TextInput, TouchableOpacity, View } from 'react-native';
import {
  applyJobUpdated,
  elapsedMs,
  formatDuration,
  parseJobList,
  subagentJobBucket,
  SUBAGENT_FILTER_LABEL,
  type JobWire,
  type SubagentJobFilter,
} from '../jobCard';
import {
  parseAgentHistory,
  parseHubSendResult,
  parseJobOutput,
  parseTaskSpawnSpawns,
  type JobWire as JobOutputWire,
  type RunnerCommand,
} from '../protocol';
import { redactSecrets } from '../redact';
import type { RunnerClient } from '../runnerClient';
import { AppBar, styles, type Palette } from '../ui';

const FILTERS: SubagentJobFilter[] = ['running', 'completed'];
const HISTORY_LINES = 200;
const HISTORY_POLL_MS = 2000;
const MESSAGE_LIMIT = 64;
const OUTPUT_LIMIT = 800;

type Page = 'jobs' | 'irc';

interface CatalogAgent {
  name: string;
  description: string;
}

interface MailboxMessage {
  id: string;
  from: string;
  to: string;
  body: string;
  timestamp: number;
  replyTo: string | null;
}

interface Owner {
  sessionId: string;
  active: boolean;
  list: number;
  historySeq: number;
  spawnHeld: boolean;
  hubHeld: boolean;
  outputHeld: boolean;
  actionHeld: boolean;
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : null;
}

function parseCatalog(value: unknown): CatalogAgent[] {
  if (!Array.isArray(value)) return [];
  const rows: CatalogAgent[] = [];
  for (const item of value) {
    const row = asRecord(item);
    if (!row || typeof row.name !== 'string' || row.name === '') continue;
    rows.push({
      name: row.name,
      description: typeof row.description === 'string' ? row.description : '',
    });
  }
  return rows;
}

function parseMailboxMessage(value: unknown): MailboxMessage | null {
  const row = asRecord(value);
  if (!row || typeof row.id !== 'string' || row.id === '') return null;
  if (typeof row.from !== 'string' || typeof row.to !== 'string' || typeof row.body !== 'string') return null;
  if (typeof row.timestamp !== 'number' || !Number.isFinite(row.timestamp)) return null;
  if (row.replyTo !== undefined && row.replyTo !== null && typeof row.replyTo !== 'string') return null;
  return {
    id: row.id,
    from: row.from,
    to: row.to,
    body: row.body,
    timestamp: row.timestamp,
    replyTo: typeof row.replyTo === 'string' ? row.replyTo : null,
  };
}

function parseMailboxMessages(value: unknown): MailboxMessage[] | null {
  if (value === undefined) return [];
  if (!Array.isArray(value)) return null;
  const rows: MailboxMessage[] = [];
  for (const item of value) {
    const message = parseMailboxMessage(item);
    if (!message) return null;
    rows.push(message);
  }
  return rows;
}

function appendMailbox(current: MailboxMessage[], next: MailboxMessage): MailboxMessage[] {
  if (current.some((row) => row.id === next.id)) return current;
  return [...current, next].slice(-MESSAGE_LIMIT);
}

function newOwner(sessionId: string): Owner {
  return {
    sessionId,
    active: true,
    list: 0,
    historySeq: 0,
    spawnHeld: false,
    hubHeld: false,
    outputHeld: false,
    actionHeld: false,
  };
}

function retireOwner(owner: Owner): void {
  owner.active = false;
  owner.sessionId = '';
}

export function JobsPanel(props: {
  palette: Palette;
  client: RunnerClient;
  sessionId: string;
  onClose: () => void;
  onError: (message: string) => void;
}): React.JSX.Element {
  const { palette } = props;
  const owner = useRef<Owner>(newOwner(props.sessionId));
  const [jobs, setJobs] = useState<JobWire[] | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [catalog, setCatalog] = useState<CatalogAgent[]>([]);
  const [messages, setMessages] = useState<MailboxMessage[]>([]);
  const [filter, setFilter] = useState<SubagentJobFilter>('running');
  const [page, setPage] = useState<Page>('jobs');
  const [now, setNow] = useState(() => Date.now());
  const [busy, setBusy] = useState(false);
  const [spawnAgent, setSpawnAgent] = useState('');
  const [spawnTask, setSpawnTask] = useState('');
  const [spawning, setSpawning] = useState(false);
  const [hubDrafts, setHubDrafts] = useState<Record<string, string>>({});
  const [hubBusy, setHubBusy] = useState(false);
  const [outputJobId, setOutputJobId] = useState<string | null>(null);
  const [outputs, setOutputs] = useState<Record<string, JobOutputWire>>({});
  const [detailsJobId, setDetailsJobId] = useState<string | null>(null);
  const [historyByAgent, setHistoryByAgent] = useState<Record<string, string>>({});

  if (owner.current.sessionId !== props.sessionId) {
    retireOwner(owner.current);
    owner.current = newOwner(props.sessionId);
  }

  const owns = (current: Owner): boolean => (
    current.active && current === owner.current && current.sessionId !== ''
  );

  const report = (label: string, error: unknown) => {
    props.onError(`${label}: ${error instanceof Error ? error.message : String(error)}`);
  };

  const rpc = async (command: RunnerCommand, sessionId: string): Promise<unknown> => {
    const response = await props.client.command(command, sessionId);
    if (!response.success) throw new Error(response.error || `${command.type} failed`);
    return response.data;
  };

  const refresh = useCallback(async (): Promise<void> => {
    const current = owner.current;
    const seq = current.list + 1;
    current.list = seq;
    setBusy(true);
    try {
      const data = await rpc({ type: 'job_list' }, current.sessionId);
      if (!owns(current) || current.list !== seq) return;
      const parsed = parseJobList(data);
      if (!parsed) throw new Error('invalid job_list');
      const mailbox = parseMailboxMessages(asRecord(data)?.messages);
      if (mailbox === null) throw new Error('invalid job_list messages');
      setJobs(parsed.jobs);
      setEnabled(parsed.enabled);
      setCatalog(parseCatalog(asRecord(data)?.catalog));
      setMessages(mailbox.slice(-MESSAGE_LIMIT));
    } catch (error) {
      if (owns(current) && current.list === seq) report('Jobs', error);
    } finally {
      if (owns(current) && current.list === seq) setBusy(false);
    }
  }, [props.client, props.onError, props.sessionId]);

  useEffect(() => {
    retireOwner(owner.current);
    const current = newOwner(props.sessionId);
    owner.current = current;
    setJobs(null);
    setEnabled(false);
    setCatalog([]);
    setMessages([]);
    setSpawnAgent('');
    setSpawnTask('');
    setHubDrafts({});
    setOutputJobId(null);
    setOutputs({});
    setDetailsJobId(null);
    setHistoryByAgent({});
    void refresh();
    return () => {
      retireOwner(current);
    };
  }, [props.sessionId, refresh]);

  useEffect(() => {
    const off = props.client.subscribeFrames((frame) => {
      const record = asRecord(frame);
      if (!record || !owner.current.active || record.sessionId !== owner.current.sessionId) return;
      if (record.type === 'job_updated') {
        setJobs((current) => (current === null ? current : applyJobUpdated(current, frame)));
        return;
      }
      if (record.type !== 'message_delivered') return;
      const message = parseMailboxMessage(record.message);
      if (!message) return;
      setMessages((current) => appendMailbox(current, message));
    });
    const tick = setInterval(() => setNow(Date.now()), 1000);
    return () => {
      off();
      clearInterval(tick);
    };
  }, [props.client]);

  const detailsJob = detailsJobId === null ? null : (jobs ?? []).find((job) => job.id === detailsJobId) ?? null;
  const detailsLive = detailsJob !== null
    && (detailsJob.status === 'queued' || detailsJob.status === 'running');

  const loadHistory = useCallback(async (agentId: string, requestSeq: number): Promise<void> => {
    const current = owner.current;
    try {
      const parsed = parseAgentHistory(await rpc({ type: 'agent_history', agentId, lines: HISTORY_LINES }, current.sessionId));
      if (!owns(current) || current.historySeq !== requestSeq) return;
      if (!parsed || parsed.agentId !== agentId) throw new Error('invalid agent_history');
      setHistoryByAgent((prev) => (prev[agentId] === parsed.text ? prev : { ...prev, [agentId]: parsed.text }));
    } catch (error) {
      if (owns(current) && current.historySeq === requestSeq) report('History', error);
    }
  }, [props.client, props.onError]);

  useEffect(() => {
    if (detailsJob === null) return;
    const agentId = detailsJob.agentId;
    if (!agentId) return;
    const current = owner.current;
    current.historySeq += 1;
    const seq = current.historySeq;
    void loadHistory(agentId, seq);
    if (!detailsLive) return;
    const timer = setInterval(() => {
      void loadHistory(agentId, owner.current.historySeq);
    }, HISTORY_POLL_MS);
    return () => {
      clearInterval(timer);
    };
  }, [detailsJob?.id, detailsJob?.agentId, detailsLive, loadHistory]);

  const visible = useMemo(() => {
    if (!jobs) return [];
    return jobs.filter((job) => subagentJobBucket(job.status) === filter);
  }, [jobs, filter]);

  const spawn = async () => {
    const current = owner.current;
    const task = spawnTask.trim();
    if (!enabled || !task || current.spawnHeld) return;
    current.spawnHeld = true;
    setSpawning(true);
    try {
      const args: Record<string, string> = { task };
      if (spawnAgent.trim()) args.agent = spawnAgent.trim();
      const spawns = parseTaskSpawnSpawns(await rpc({ type: 'task_spawn', args }, current.sessionId));
      if (!owns(current)) return;
      if (spawns.length === 0) throw new Error('task_spawn returned no jobs');
      setSpawnTask('');
      await refresh();
    } catch (error) {
      if (owns(current)) report('Spawn', error);
    } finally {
      current.spawnHeld = false;
      if (owns(current)) setSpawning(false);
    }
  };

  const stopJob = async (jobId: string) => {
    const current = owner.current;
    if (current.actionHeld) return;
    current.actionHeld = true;
    try {
      await rpc({ type: 'job_cancel', jobIds: [jobId] }, current.sessionId);
      if (owns(current)) await refresh();
    } catch (error) {
      if (owns(current)) report('Stop', error);
    } finally {
      current.actionHeld = false;
    }
  };

  const sendHub = async (job: JobWire) => {
    const current = owner.current;
    const body = (hubDrafts[job.id] ?? '').trim();
    if (!enabled || !body || !job.agentId || current.hubHeld) return;
    current.hubHeld = true;
    setHubBusy(true);
    try {
      const result = parseHubSendResult(await rpc({ type: 'hub_send', to: job.agentId, body }, current.sessionId));
      if (!owns(current)) return;
      if (!result) throw new Error('invalid hub_send');
      const failed = result.receipts.find((receipt) => receipt.error);
      if (failed) throw new Error(failed.error ?? 'hub_send failed');
      setHubDrafts((prev) => ({ ...prev, [job.id]: '' }));
      await refresh();
    } catch (error) {
      if (owns(current)) report('Hub', error);
    } finally {
      current.hubHeld = false;
      if (owns(current)) setHubBusy(false);
    }
  };

  const viewOutput = async (jobId: string) => {
    const current = owner.current;
    if (outputJobId === jobId) {
      setOutputJobId(null);
      return;
    }
    if (current.outputHeld) return;
    current.outputHeld = true;
    try {
      const parsed = parseJobOutput(await rpc({ type: 'job_output', jobId }, current.sessionId));
      if (!owns(current)) return;
      if (!parsed || parsed.job.id !== jobId) throw new Error('invalid job_output');
      setOutputs((prev) => ({ ...prev, [jobId]: parsed.job }));
      setOutputJobId(jobId);
    } catch (error) {
      if (owns(current)) report('Output', error);
    } finally {
      current.outputHeld = false;
    }
  };

  const row = { flexDirection: 'row' as const, flexWrap: 'wrap' as const, gap: 12 };

  return (
    <View style={[styles.screen, { backgroundColor: palette.bg }]}>
      <AppBar
        palette={palette}
        title="Jobs"
        onBack={props.onClose}
        actions={[{ icon: 'refresh', label: 'Refresh', onPress: () => void refresh(), disabled: busy }]}
      />
      <View style={{ flexDirection: 'row', borderBottomWidth: 1, borderBottomColor: palette.border }}>
        {(['jobs', 'irc'] as const).map((id) => (
          <TouchableOpacity
            key={id}
            style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: page === id ? palette.accent : palette.surface }]}
            onPress={() => setPage(id)}
            accessibilityLabel={id === 'jobs' ? 'Jobs page' : 'IRC page'}>
            <Text style={[styles.primaryButtonText, { color: page === id ? palette.bg : palette.fg }]}>
              {id === 'jobs' ? 'Jobs' : 'IRC'}
            </Text>
          </TouchableOpacity>
        ))}
      </View>
      <ScrollView style={styles.list}>
        {!enabled && (
          <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>
            Orchestration not enabled
          </Text>
        )}
        {page === 'irc' && (
          <>
            {messages.length === 0 && (
              <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No hub mailbox messages.</Text>
            )}
            {messages.map((message) => (
              <View key={message.id} style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border }]}>
                <View style={styles.rowBody}>
                  <Text style={[styles.rowTitle, { color: palette.fg }]} numberOfLines={2}>
                    {`${redactSecrets(message.from)} ➤ ${redactSecrets(message.to)}`}
                  </Text>
                  <Text style={[styles.rowSub, { color: palette.muted }]} numberOfLines={4}>
                    {redactSecrets(message.body).slice(0, OUTPUT_LIMIT)}
                  </Text>
                </View>
              </View>
            ))}
          </>
        )}
        {page === 'jobs' && (
          <>
            <View style={{ paddingHorizontal: 16, paddingTop: 12, gap: 12 }}>
              <View style={row}>
                {catalog.map((agent) => (
                  <TouchableOpacity
                    key={agent.name}
                    accessibilityLabel={`Catalog ${agent.name}`}
                    onPress={() => setSpawnAgent(agent.name)}
                    style={[styles.cardChip, { borderColor: spawnAgent === agent.name ? palette.accent : palette.border }]}>
                    <Text style={{ color: spawnAgent === agent.name ? palette.accent : palette.fg }}>{redactSecrets(agent.name)}</Text>
                  </TouchableOpacity>
                ))}
              </View>
              <TextInput
                style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
                value={spawnAgent}
                onChangeText={setSpawnAgent}
                placeholder="agent (auto)"
                accessibilityLabel="Spawn agent"
                placeholderTextColor={palette.muted}
              />
              <TextInput
                style={[styles.input, { backgroundColor: palette.surface, borderColor: palette.border, color: palette.fg }]}
                value={spawnTask}
                onChangeText={setSpawnTask}
                placeholder="task briefing"
                accessibilityLabel="Spawn task"
                placeholderTextColor={palette.muted}
              />
              <TouchableOpacity
                accessibilityLabel="Spawn"
                style={[styles.primaryButton, { backgroundColor: palette.accent }, (!enabled || spawning || !spawnTask.trim()) && styles.faded]}
                disabled={!enabled || spawning || !spawnTask.trim()}
                onPress={() => void spawn()}>
                <Text style={[styles.primaryButtonText, { color: palette.bg }]}>{spawning ? 'Spawning…' : 'Spawn'}</Text>
              </TouchableOpacity>
            </View>
            <View style={{ flexDirection: 'row', borderBottomWidth: 1, borderBottomColor: palette.border }}>
              {FILTERS.map((id) => (
                <TouchableOpacity
                  key={id}
                  style={[styles.primaryButton, { flex: 1, marginTop: 0, backgroundColor: filter === id ? palette.accent : palette.surface }]}
                  onPress={() => setFilter(id)}
                  accessibilityLabel={SUBAGENT_FILTER_LABEL[id]}>
                  <Text style={[styles.primaryButtonText, { color: filter === id ? palette.bg : palette.fg }]}>
                    {SUBAGENT_FILTER_LABEL[id]}
                  </Text>
                </TouchableOpacity>
              ))}
            </View>
            {jobs === null && (
              <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>{busy ? '…' : '--'}</Text>
            )}
            {jobs && jobs.length === 0 && (
              <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No live jobs</Text>
            )}
            {jobs && jobs.length > 0 && visible.length === 0 && (
              <Text style={[styles.emptyText, { color: palette.muted, padding: 24 }]}>No jobs</Text>
            )}
            {visible.map((job) => {
              const elapsed = elapsedMs(job, now);
              const label = redactSecrets(job.description || job.agent || job.id);
              const output = outputs[job.id];
              const history = historyByAgent[job.agentId] ?? '';
              const live = job.status === 'running' || job.status === 'queued';
              return (
                <View key={job.id} style={[styles.listRow, { backgroundColor: palette.surface, borderBottomColor: palette.border, flexDirection: 'column', alignItems: 'stretch' }]}>
                  <View style={row}>
                    <View style={styles.rowBody}>
                      <Text
                        style={[styles.rowTitle, { color: palette.fg }]}
                        numberOfLines={detailsJobId === job.id ? undefined : 2}
                        selectable={detailsJobId === job.id}
                      >
                        {label}
                      </Text>
                      <Text style={[styles.rowSub, { color: palette.muted }]}>
                        {job.status}{elapsed != null ? ` · ${formatDuration(elapsed)}` : ''}
                      </Text>
                    </View>
                    {live && (
                      <TouchableOpacity onPress={() => void stopJob(job.id)} accessibilityLabel={`Stop ${job.id}`}>
                        <Text style={{ color: palette.error, fontSize: 16 }}>{'■'} Stop</Text>
                      </TouchableOpacity>
                    )}
                  </View>
                  <View style={row}>
                    <TouchableOpacity accessibilityLabel={`Details ${job.id}`} onPress={() => setDetailsJobId((currentId) => currentId === job.id ? null : job.id)}>
                      <Text style={{ color: palette.accent }}>{detailsJobId === job.id ? 'Hide details' : 'Details'}</Text>
                    </TouchableOpacity>
                    <TouchableOpacity accessibilityLabel={outputJobId === job.id ? `Hide output ${job.id}` : `Output ${job.id}`} onPress={() => void viewOutput(job.id)}>
                      <Text style={{ color: palette.accent }}>{outputJobId === job.id ? 'Hide output' : 'Output'}</Text>
                    </TouchableOpacity>
                  </View>
                  {outputJobId === job.id && (
                    <Text style={[styles.rowSub, { color: palette.fg }]} accessibilityLabel={`Output body ${job.id}`}>
                      {output?.result?.error
                        ? redactSecrets(output.result.error).slice(0, OUTPUT_LIMIT)
                        : output?.result && output.result.output.trim() !== ''
                          ? redactSecrets(output.result.output).slice(0, OUTPUT_LIMIT)
                          : '(no delivered output yet)'}
                    </Text>
                  )}
                  {detailsJobId === job.id && (
                    <Text style={[styles.rowSub, { color: palette.fg }]} accessibilityLabel={`History ${job.id}`}>
                      {history ? redactSecrets(history) : '--'}
                    </Text>
                  )}
                  <TextInput
                    style={[styles.input, { backgroundColor: palette.bg, borderColor: palette.border, color: palette.fg }]}
                    value={hubDrafts[job.id] ?? ''}
                    onChangeText={(value) => setHubDrafts((prev) => ({ ...prev, [job.id]: value }))}
                    placeholder={`message ${job.agentId}`}
                    accessibilityLabel={`Hub ${job.id}`}
                    placeholderTextColor={palette.muted}
                  />
                  <TouchableOpacity
                    accessibilityLabel={`Send hub ${job.id}`}
                    style={[styles.primaryButton, { backgroundColor: palette.accent }, (!enabled || hubBusy || !(hubDrafts[job.id] ?? '').trim()) && styles.faded]}
                    disabled={!enabled || hubBusy || !(hubDrafts[job.id] ?? '').trim()}
                    onPress={() => void sendHub(job)}>
                    <Text style={[styles.primaryButtonText, { color: palette.bg }]}>Send hub</Text>
                  </TouchableOpacity>
                </View>
              );
            })}
          </>
        )}
        <View style={styles.bottomPad} />
      </ScrollView>
    </View>
  );
}
