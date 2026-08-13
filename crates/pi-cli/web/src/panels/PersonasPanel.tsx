// Personas panel — persistent persona definitions with the full lifecycle:
// list/view, create/edit (reusing the backend schema/validation/storage/
// reload), remove vs purge (both require an explicit confirmation dialog),
// select as the preferred agent, and run (task_spawn with the persona's agent
// name). Terminology and behavior mirror the TUI `/persona` surface.
//
// RPC surface (crates/pi-cli/src/modes/rpc.rs): persona_list / persona_get /
// persona_create / persona_edit / persona_remove / persona_purge /
// persona_select / persona_clear / persona_current + the existing task_spawn
// for Run. Every wire string passes through safeText() at render; the
// definition body is shown as literal text (never injected as HTML).
//
// Natural-language persona invocation ("让 <persona> …") is resolved by the
// MAIN agent through the orchestration agent catalog/task tool — this panel
// only lists the same persisted definitions and never heuristically routes
// prompts.

import { useEffect, useRef, useState, type KeyboardEvent, type RefObject } from 'react';
import { safeText } from '../redact';
import {
  NL_INVOCATION_NOTE,
  PERSONA_EDIT_MAX_UNITS,
  PURGE_NOTE,
  REMOVE_NOTE,
  buildPersonaCreateContent,
  buildPersonaRunCommand,
  declaredFrontmatterName,
  isEditableContent,
  parsePersonaDetail,
  parsePersonaList,
  parseSpawns,
  persistenceLine,
  responseMessage,
  sourceLabel,
  validatePersonaName,
  type PersonaDetailWire,
  type PersonaRowWire,
} from '../personas';

interface PersonasPanelProps {
  sendCommand: (command: Record<string, unknown>) => Promise<unknown>;
  onClose: () => void;
}

type EditorState =
  | { mode: 'new'; name: string; content: string }
  | { mode: 'edit'; name: string; content: string }
  | null;

type ConfirmState = { name: string; kind: 'remove' | 'purge' } | null;

export function PersonasPanel({ sendCommand, onClose }: PersonasPanelProps) {
  const [rows, setRows] = useState<PersonaRowWire[]>([]);
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [toast, setToast] = useState('');
  // Detail modal: persona under inspection.
  const [viewName, setViewName] = useState<string | null>(null);
  const [detail, setDetail] = useState<PersonaDetailWire | null>(null);
  const [viewError, setViewError] = useState('');
  const viewTriggerRef = useRef<HTMLElement | null>(null);
  const viewCloseRef = useRef<HTMLButtonElement | null>(null);
  const viewRef = useRef<HTMLDivElement | null>(null);
  // Create/edit modal.
  const [editor, setEditor] = useState<EditorState>(null);
  const [editorError, setEditorError] = useState('');
  const editorCloseRef = useRef<HTMLButtonElement | null>(null);
  const editorRef = useRef<HTMLDivElement | null>(null);
  // Remove/purge confirmation modal (explicit second click required).
  const [confirm, setConfirm] = useState<ConfirmState>(null);
  const confirmCloseRef = useRef<HTMLButtonElement | null>(null);
  const confirmRef = useRef<HTMLDivElement | null>(null);
  // Per-row run task drafts + which row has the inline run input open.
  const [runDrafts, setRunDrafts] = useState<Record<string, string>>({});
  const [runOpen, setRunOpen] = useState<string | null>(null);

  const load = () => {
    setError('');
    sendCommand({ type: 'persona_list' })
      .then((data) => {
        const payload = (data || {}) as { enabled?: unknown };
        setEnabled(payload.enabled === true);
        setRows(parsePersonaList(data));
      })
      .catch((err: Error) => {
        setError(`persona_list failed: ${err.message}`);
        setEnabled(false);
        setRows([]);
      });
  };

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sendCommand]);

  const openView = (name: string) => {
    viewTriggerRef.current = document.activeElement as HTMLElement | null;
    setDetail(null);
    setViewError('');
    setViewName(name);
    sendCommand({ type: 'persona_get', name })
      .then((data) => {
        const parsed = parsePersonaDetail(data);
        if (parsed) {
          setDetail(parsed);
        } else {
          setViewError('persona_get returned an unreadable definition');
        }
      })
      .catch((err: Error) => setViewError(err.message || 'persona_get failed'));
  };

  const closeView = () => {
    setViewName(null);
    setDetail(null);
    const trigger = viewTriggerRef.current;
    viewTriggerRef.current = null;
    if (trigger && typeof trigger.focus === 'function') trigger.focus();
  };

  const openNew = () => {
    setEditorError('');
    setEditor({ mode: 'new', name: '', content: '' });
  };

  const openEdit = (name: string) => {
    setEditorError('');
    setEditor({ mode: 'edit', name, content: '' });
    sendCommand({ type: 'persona_get', name })
      .then((data) => {
        const parsed = parsePersonaDetail(data);
        if (parsed) {
          // Seed the definition only when the user has NOT already typed: the
          // async fetch must never clobber an in-progress draft.
          setEditor((current) =>
            current && current.mode === 'edit' && current.name === name && current.content === ''
              ? { mode: 'edit', name, content: parsed.content }
              : current,
          );
        } else {
          setEditorError('persona_get returned an unreadable definition');
        }
      })
      .catch((err: Error) => setEditorError(err.message || 'persona_get failed'));
  };

  const closeEditor = () => {
    setEditor(null);
    setEditorError('');
  };

  const editorNameError = () => {
    if (!editor) return null;
    if (editor.mode === 'new') return validatePersonaName(editor.name);
    return null;
  };

  // SOFT name-agreement hint only: the backend is authoritative and rejects
  // mismatched names on save (mirroring `parse_frontmatter`/`unquote`, which
  // also accept quoted names — the frontend parser never blocks a backend-
  // legal file). Returns null when the frontmatter name cannot be resolved.
  const editorContentError = () => {
    if (!editor) return null;
    const declared = declaredFrontmatterName(editor.content);
    if (declared === null) return null;
    if (editor.mode === 'new' && editor.name !== '' && editorNameError() === null) {
      if (declared !== editor.name) {
        return 'frontmatter name must match the persona name (the backend will also reject a mismatch)';
      }
    }
    if (editor.mode === 'edit' && declared !== editor.name) {
      return 'frontmatter name must match the target name (renames are rejected)';
    }
    return null;
  };

  const saveEditor = () => {
    if (!editor) return;
    const nameError = editorNameError();
    if (nameError) {
      setEditorError(nameError);
      return;
    }
    if (editor.content.trim() === '') {
      setEditorError('persona definition content is required');
      return;
    }
    setBusy(true);
    setEditorError('');
    const command =
      editor.mode === 'new'
        ? { type: 'persona_create', name: editor.name, content: editor.content }
        : { type: 'persona_edit', name: editor.name, content: editor.content };
    sendCommand(command)
      .then((data) => {
        const message =
          responseMessage(data) || (editor?.mode === 'new' ? 'created' : 'edited');
        setToast(safeText(message));
        setEditor(null);
        load();
      })
      // The backend is authoritative: a mismatched frontmatter name, invalid
      // content, or a missing definition surfaces HERE as an error and the
      // editor stays open with the draft intact.
      .catch((err: Error) => setEditorError(err.message || 'save failed'))
      .finally(() => setBusy(false));
  };

  const selectPersona = (name: string) => {
    setError('');
    sendCommand({ type: 'persona_select', name })
      .then((data) => {
        const message = responseMessage(data) || `${name} selected`;
        setToast(safeText(message));
        load();
      })
      .catch((err: Error) => setError(`persona_select failed: ${err.message}`));
  };

  const runPersona = (name: string) => {
    const task = (runDrafts[name] || '').trim();
    if (!task) return;
    setBusy(true);
    setError('');
    sendCommand(buildPersonaRunCommand(name, task))
      .then((data) => {
        const first = parseSpawns(data)[0];
        // A spawn that produced no job is a FAILURE, not a success: surface
        // the error and keep the assignment so the user can retry.
        if (!first) {
          throw new Error('task_spawn returned no jobs');
        }
        setToast(`spawned ${name} as job ${first.jobId || '(unknown)'}`);
        setRunDrafts((prev) => ({ ...prev, [name]: '' }));
        setRunOpen(null);
      })
      .catch((err: Error) => setError(`task_spawn failed: ${err.message}`))
      .finally(() => setBusy(false));
  };

  const requestDestructive = (name: string, kind: 'remove' | 'purge') => {
    setConfirm({ name, kind });
  };

  const confirmDestructive = (kind: 'remove' | 'purge') => {
    if (!confirm) return;
    const command =
      kind === 'remove'
        ? { type: 'persona_remove', name: confirm.name, confirm: true }
        : { type: 'persona_purge', name: confirm.name, confirm: true };
    const label = kind === 'remove' ? 'removed' : 'purged';
    setBusy(true);
    setError('');
    sendCommand(command)
      .then((data) => {
        const message = responseMessage(data) || `${confirm?.name} ${label}`;
        setToast(safeText(message));
        setConfirm(null);
        load();
      })
      .catch((err: Error) => setError(`persona_${kind} failed: ${err.message}`))
      .finally(() => setBusy(false));
  };

  const clearPreference = () => {
    setError('');
    sendCommand({ type: 'persona_clear' })
      .then((data) => {
        const message = responseMessage(data) || 'preference cleared';
        setToast(safeText(message));
        load();
      })
      .catch((err: Error) => setError(`persona_clear failed: ${err.message}`));
  };

  // Dialog keyboard: Escape closes; Tab traps focus inside the dialog.
  const dialogKeyDown = (close: () => void, ref: RefObject<HTMLDivElement | null>) => {
    return (e: KeyboardEvent<HTMLDivElement>) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        e.stopPropagation();
        close();
        return;
      }
      if (e.key !== 'Tab' || !ref.current) return;
      const focusables = ref.current.querySelectorAll<HTMLElement>(
        'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
      );
      if (focusables.length === 0) return;
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
  };

  // Initial focus lands on the Close button of each modal when it opens.
  useEffect(() => {
    if (viewName) viewCloseRef.current?.focus();
  }, [viewName]);
  useEffect(() => {
    if (editor) editorCloseRef.current?.focus();
  }, [editor]);
  useEffect(() => {
    if (confirm) confirmCloseRef.current?.focus();
  }, [confirm]);

  const preferredName = rows.find((row) => row.preferred)?.name ?? null;

  // Derived, never stale state: a large seeded definition (over the editable
  // bound) is shown read-only; anything else is editable. A fetch landing
  // after the user typed can therefore never flip the editor read-only or
  // clobber the draft (openEdit seeds only an untouched editor).
  const editorReadOnly =
    !!editor && editor.mode === 'edit' && editor.content !== '' && !isEditableContent(editor.content);
  const editorSaveDisabled =
    busy || editorReadOnly || editorNameError() !== null || !editor || editor.content.trim() === '';

  return (
    <aside id="personas-panel" className="subagents-panel personas-panel" aria-label="Personas panel">
      <div className="subagents-panel__head">
        <span className="subagents-panel__title">Personas</span>
        <span className="subagents-panel__counts" title="persistent persona definitions">
          {rows.length === 0 ? 'no personas' : `${rows.length} ${rows.length === 1 ? 'persona' : 'personas'}`}
        </span>
        <button
          id="personas-close-btn"
          type="button"
          className="subagents-panel__close"
          title="Close personas panel"
          aria-label="Close personas panel"
          onClick={onClose}
        >
          ×
        </button>
      </div>

      {!enabled && (
        <div className="subagents-panel__empty">
          Persona catalog unavailable in this session (no resource manager attached).
        </div>
      )}

      {enabled && (
        <>
          <div className="personas-panel__toolbar">
            <button
              id="personas-new-btn"
              type="button"
              onClick={openNew}
              disabled={busy}
              title="Create a new persona definition (mirrors /persona new)"
            >
              New persona
            </button>
            <button
              id="personas-refresh-btn"
              type="button"
              onClick={load}
              disabled={busy}
              title="Reload the persona catalog"
            >
              Refresh
            </button>
            {preferredName ? (
              <button
                id="personas-clear-pref-btn"
                type="button"
                onClick={clearPreference}
                disabled={busy}
                title="Clear the preferred persona (mirrors /persona --clear)"
              >
                Clear selected
              </button>
            ) : null}
          </div>

          <div className="personas-panel__list" id="personas-list">
            {rows.length === 0 && (
              <div className="subagents-panel__empty" data-personas-empty>
                No personas loaded. Create one above, or drop a persona.md under
                personas/&lt;name&gt;/ in the agent directory (TUI: /persona new &lt;name&gt;).
              </div>
            )}
            {rows.map((row) => (
              <section
                key={row.name}
                className="persona-row"
                data-persona-name={row.name}
                data-preferred={row.preferred ? 'true' : 'false'}
              >
                <div className="persona-row__head">
                  <span className="persona-row__title">
                    {row.preferred ? '★ ' : ''}
                    {safeText(row.name)}
                  </span>
                  <span className="persona-row__source">{sourceLabel(row.source)}</span>
                </div>
                <div className="persona-row__description" title={safeText(row.description)}>
                  {safeText(row.description)}
                </div>
                <div className="persona-row__contract" title={safeText(row.contractSummary)}>
                  {row.contractSummary ? safeText(row.contractSummary) : '(default contract)'}
                </div>
                <div className="persona-row__persistence" data-persona-persistence>
                  {persistenceLine(row)}
                </div>
                <div className="persona-row__actions">
                  <button
                    type="button"
                    data-action="view"
                    onClick={() => openView(row.name)}
                    title="View the persona definition and persistence state"
                  >
                    View
                  </button>
                  <button
                    type="button"
                    data-action="edit"
                    onClick={() => openEdit(row.name)}
                    title="Edit persona.md (mirrors /persona edit)"
                  >
                    Edit
                  </button>
                  <button
                    type="button"
                    data-action="select"
                    onClick={() => selectPersona(row.name)}
                    disabled={row.preferred}
                    title="Prefer this persona for unnamed task spawns (mirrors /persona <name> --select)"
                  >
                    {row.preferred ? 'Selected' : 'Select'}
                  </button>
                  <button
                    type="button"
                    data-action="run"
                    onClick={() => setRunOpen(runOpen === row.name ? null : row.name)}
                    title="Spawn a task with this persona as the agent (task_spawn)"
                  >
                    Run
                  </button>
                  <button
                    type="button"
                    data-action="remove"
                    onClick={() => requestDestructive(row.name, 'remove')}
                    title="Delete persona.md, keeping memory and sessions (requires confirmation)"
                  >
                    Remove…
                  </button>
                  <button
                    type="button"
                    data-action="purge"
                    onClick={() => requestDestructive(row.name, 'purge')}
                    title="Delete the whole persona root, memory and sessions included (requires confirmation)"
                  >
                    Purge…
                  </button>
                </div>
                {runOpen === row.name && (
                  <div className="persona-row__run" data-persona-run>
                    <input
                      id={`persona-run-input-${row.name}`}
                      placeholder={`assignment for ${safeText(row.name)} (task spawn)`}
                      value={runDrafts[row.name] || ''}
                      onChange={(e) =>
                        setRunDrafts((prev) => ({ ...prev, [row.name]: e.target.value }))
                      }
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault();
                          runPersona(row.name);
                        }
                      }}
                      spellCheck={false}
                    />
                    <button
                      id={`persona-run-start-${row.name}`}
                      type="button"
                      onClick={() => runPersona(row.name)}
                      disabled={busy || !(runDrafts[row.name] || '').trim()}
                    >
                      Start
                    </button>
                  </div>
                )}
              </section>
            ))}
          </div>

          <div className="personas-panel__note" data-personas-nl-note>
            {NL_INVOCATION_NOTE}
          </div>
        </>
      )}

      {error && (
        <div className="subagents-panel__error" data-panel-error>
          {safeText(error)}
        </div>
      )}
      {toast && (
        <div className="subagents-panel__toast" data-panel-toast>
          {safeText(toast)}
        </div>
      )}

      {/* --- Detail modal: definition + persistence semantics, literal text. --- */}
      {viewName && (
        <div className="subagent-details-backdrop" onClick={closeView}>
          <div
            ref={viewRef}
            className="subagent-details personas-detail"
            role="dialog"
            aria-modal="true"
            aria-label="Persona details"
            data-persona-detail
            data-persona-name={viewName}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={dialogKeyDown(closeView, viewRef)}
          >
            <div className="subagent-details__head">
              <span className="subagent-details__title">{safeText(viewName)}</span>
              <div className="subagent-details__head-actions">
                <button
                  ref={viewCloseRef}
                  type="button"
                  className="subagent-details__action"
                  data-persona-detail-close
                  onClick={closeView}
                  title="Close details (Esc)"
                >
                  Close
                </button>
              </div>
            </div>
            {viewError !== '' ? (
              <div className="subagent-details__error" data-details-error>
                {safeText(viewError)}
              </div>
            ) : detail ? (
              <>
                <div className="subagent-details__meta" data-persona-meta>
                  <span className="persona-row__source">{sourceLabel(detail.source)}</span>
                  {detail.preferred ? <span data-persona-preferred>selected</span> : null}
                  {detail.trusted ? null : <span data-persona-untrusted>untrusted</span>}
                </div>
                <div className="subagent-details__section">
                  <div className="subagent-details__label">Description</div>
                  <div className="subagent-details__description" data-persona-description>
                    {safeText(detail.description)}
                  </div>
                </div>
                <div className="subagent-details__section">
                  <div className="subagent-details__label">Contract</div>
                  <div className="subagent-details__activity" data-persona-contract>
                    {detail.contractSummary ? safeText(detail.contractSummary) : '(default contract)'}
                  </div>
                </div>
                <div className="subagent-details__section">
                  <div className="subagent-details__label">Persistence</div>
                  <div className="subagent-details__activity" data-persona-persistence-detail>
                    {persistenceLine(detail)} — durable state under the persona root;
                    remove keeps it, purge deletes it.
                  </div>
                </div>
                <div className="subagent-details__section subagent-details__section--transcript">
                  <div className="subagent-details__label">Definition (persona.md)</div>
                  <pre className="subagent-details__history" data-persona-content>
                    {safeText(detail.content)}
                  </pre>
                  {detail.contentTruncated && (
                    <div className="subagent-details__error">
                      definition truncated for display; use /persona edit for the full file
                    </div>
                  )}
                </div>
              </>
            ) : (
              <div className="subagent-details__history">(loading definition…)</div>
            )}
          </div>
        </div>
      )}

      {/* --- Create/edit modal. --- */}
      {editor && (
        <div className="subagent-details-backdrop" onClick={closeEditor}>
          <div
            ref={editorRef}
            className="subagent-details personas-editor"
            role="dialog"
            aria-modal="true"
            aria-label={editor.mode === 'new' ? 'New persona' : `Edit persona ${editor.name}`}
            data-persona-editor
            data-mode={editor.mode}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={dialogKeyDown(closeEditor, editorRef)}
          >
            <div className="subagent-details__head">
              <span className="subagent-details__title">
                {editor.mode === 'new' ? 'New persona' : `Edit ${safeText(editor.name)}`}
              </span>
              <div className="subagent-details__head-actions">
                <button
                  ref={editorCloseRef}
                  type="button"
                  className="subagent-details__action"
                  data-persona-editor-close
                  onClick={closeEditor}
                  title="Cancel (Esc)"
                >
                  Cancel
                </button>
              </div>
            </div>
            <div className="subagent-details__section">
              <div className="subagent-details__label">Name</div>
              {editor.mode === 'new' ? (
                <input
                  id="persona-create-name"
                  className="personas-editor__input"
                  value={editor.name}
                  onChange={(e) => {
                    const name = e.target.value;
                    setEditor((current) => (current ? { ...current, name } : current));
                  }}
                  placeholder="1..64 ASCII letters/digits/_/-"
                  spellCheck={false}
                  aria-invalid={editorNameError() !== null}
                />
              ) : (
                <div className="subagent-details__activity" data-persona-editor-name>
                  {safeText(editor.name)} (renames are rejected; remove + new to rename)
                </div>
              )}
            </div>
            <div className="subagent-details__section subagent-details__section--transcript">
              <div className="subagent-details__label">
                Definition (persona.md)
                {editor.mode === 'new' && (
                  <button
                    type="button"
                    className="subagent-details__action personas-editor__template"
                    onClick={() => {
                      const name = editor.name.trim();
                      const nameError = validatePersonaName(name);
                      if (nameError) {
                        setEditorError(nameError);
                        return;
                      }
                      setEditorError('');
                      setEditor((current) =>
                        current ? { ...current, content: buildPersonaCreateContent(name) } : current,
                      );
                    }}
                    disabled={validatePersonaName(editor.name) !== null}
                    title="Seed the editor with the persona template"
                  >
                    Use template
                  </button>
                )}
              </div>
              <textarea
                id={editor.mode === 'new' ? 'persona-create-content' : 'persona-edit-content'}
                className="personas-editor__content"
                value={editor.content}
                onChange={(e) =>
                  setEditor((current) => (current ? { ...current, content: e.target.value } : current))
                }
                spellCheck={false}
                maxLength={PERSONA_EDIT_MAX_UNITS}
                readOnly={editorReadOnly}
                aria-invalid={editorContentError() !== null}
              />
              {editorContentError() && (
                <div className="subagent-details__error" data-persona-editor-content-error>
                  {safeText(editorContentError())}
                </div>
              )}
              {editorReadOnly && (
                <div className="subagent-details__error" data-persona-readonly-note>
                  This definition is too large for the Web editor (over 64 KiB); it is shown
                  read-only — use /persona edit in the TUI to change it without truncation.
                </div>
              )}
            </div>
            {editorError && (
              <div className="subagent-details__error" data-persona-editor-error>
                {safeText(editorError)}
              </div>
            )}
            <div className="subagent-details__head-actions personas-editor__save-row">
              <button
                id="persona-editor-save"
                type="button"
                className="subagent-details__action"
                onClick={saveEditor}
                disabled={editorSaveDisabled}
                data-save-disabled-reason={
                  editorSaveDisabled
                    ? [
                        busy ? 'busy' : null,
                        editorReadOnly ? 'readOnly' : null,
                        editorNameError() !== null ? 'nameError' : null,
                        editor.content.trim() === '' ? 'empty' : null,
                      ]
                        .filter(Boolean)
                        .join(',')
                    : undefined
                }
              >
                {editor.mode === 'new' ? 'Create' : 'Save'}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* --- Remove/purge confirmation modal: clearly distinct + explicit. --- */}
      {confirm && (
        <div className="subagent-details-backdrop" onClick={() => setConfirm(null)}>
          <div
            ref={confirmRef}
            className="subagent-details personas-confirm"
            role="dialog"
            aria-modal="true"
            aria-label={`${confirm.kind === 'remove' ? 'Remove' : 'Purge'} persona ${confirm.name}`}
            data-persona-confirm
            data-kind={confirm.kind}
            data-persona-name={confirm.name}
            onClick={(e) => e.stopPropagation()}
            onKeyDown={dialogKeyDown(() => setConfirm(null), confirmRef)}
          >
            <div className="subagent-details__head">
              <span className="subagent-details__title">
                {confirm.kind === 'remove' ? `Remove ${safeText(confirm.name)}?` : `Purge ${safeText(confirm.name)}?`}
              </span>
              <div className="subagent-details__head-actions">
                <button
                  ref={confirmCloseRef}
                  type="button"
                  className="subagent-details__action"
                  data-persona-confirm-cancel
                  onClick={() => setConfirm(null)}
                  title="Cancel (Esc)"
                >
                  Cancel
                </button>
              </div>
            </div>
            <div className="subagent-details__section">
              <div className="subagent-details__label">Remove vs purge</div>
              <div className="subagent-details__activity" data-persona-confirm-remove-note>
                Remove: {REMOVE_NOTE}
              </div>
              <div className="subagent-details__activity" data-persona-confirm-purge-note>
                Purge: {PURGE_NOTE}
              </div>
            </div>
            <div className="subagent-details__head-actions personas-confirm__actions">
              <button
                type="button"
                className="subagent-details__action personas-confirm__remove"
                data-persona-confirm-remove
                onClick={() => confirmDestructive('remove')}
                disabled={busy}
              >
                Remove definition (keep state)
              </button>
              <button
                type="button"
                className="subagent-details__action personas-confirm__purge"
                data-persona-confirm-purge
                onClick={() => confirmDestructive('purge')}
                disabled={busy}
              >
                Purge root (delete everything)
              </button>
            </div>
          </div>
        </div>
      )}
    </aside>
  );
}
