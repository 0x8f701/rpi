use std::{
    collections::BTreeSet,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use futures_util::FutureExt;
use parking_lot::RwLock as StdRwLock;
use pi_ai::{
    AfterProviderResponseHook, AssistantMessage, BeforeProviderHeadersHook,
    BeforeProviderRequestHook, ContentBlock, Message, Model, PayloadHook, ResponseHook,
    SimpleStreamOptions, StopReason, ThinkingBudgets, Transport, Usage, UserMessage,
};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::{
    AbortController, AbortSignal, AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig,
    AgentMessage, AgentTool, BeforeAgentStartContext, BeforeAgentStartFn, BeforeToolCallFn,
    ConvertToLlmFn, GetApiKeyFn, Listener, PrepareNextTurnFn, QueueMode, StreamFn, ThinkingLevel,
    ToolExecutionMode, TransformContextFn, TransformMessageFn, queue::PendingQueue, run_agent_loop,
    run_agent_loop_continue,
};

#[derive(Clone, Debug)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<AgentTool>,
    pub messages: Vec<AgentMessage>,
    pub is_streaming: bool,
    pub streaming_message: Option<AgentMessage>,
    pub pending_tool_calls: BTreeSet<String>,
    pub error_message: Option<String>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),
            model: Model::default(),
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: BTreeSet::new(),
            error_message: None,
        }
    }
}

#[derive(Clone)]
pub struct AgentOptions {
    pub initial_state: AgentState,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub transform_context: Option<TransformContextFn>,
    pub before_agent_start: Option<BeforeAgentStartFn>,
    pub transform_message: Option<TransformMessageFn>,
    pub stream_fn: StreamFn,
    pub get_api_key: Option<GetApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub stream_options: SimpleStreamOptions,
    pub tool_execution: ToolExecutionMode,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            initial_state: AgentState::default(),
            convert_to_llm: None,
            transform_context: None,
            before_agent_start: None,
            transform_message: None,
            stream_fn: Arc::new(|model, context, options| {
                Box::pin(pi_ai::stream_simple(model, context, options))
            }),
            get_api_key: None,
            before_tool_call: None,
            after_tool_call: None,
            prepare_next_turn: None,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            stream_options: SimpleStreamOptions::default(),
            tool_execution: ToolExecutionMode::Parallel,
        }
    }
}

struct ActiveRun {
    controller: AbortController,
    signal: AbortSignal,
    generation: u64,
    terminal_failure_finalized: bool,
}

struct Inner {
    state: RwLock<AgentState>,
    listeners: StdRwLock<Vec<Option<Listener>>>,
    steering: Mutex<PendingQueue>,
    follow_up: Mutex<PendingQueue>,
    active: Mutex<Option<ActiveRun>>,
    /// Monotonic run generation. Each claim mints a fresh, unique generation
    /// so a stale drop cleanup can verify it still owns the current run before
    /// clearing state/removing `active`, and never touches a newer claim.
    run_generation: AtomicU64,
    idle: Notify,
    stream_options: RwLock<SimpleStreamOptions>,
    options: AgentOptions,
    before_agent_start: StdRwLock<Option<BeforeAgentStartFn>>,
    transform_message: StdRwLock<Option<TransformMessageFn>>,
    transform_context: StdRwLock<Option<TransformContextFn>>,
    before_tool_call: StdRwLock<Option<BeforeToolCallFn>>,
    after_tool_call: StdRwLock<Option<AfterToolCallFn>>,
    /// Test seam: when set, `release_claim_owned` notifies `parked` and then
    /// awaits `gate` after verifying ownership and before clearing state, so
    /// tests can synchronously know a release is in flight and cancel it
    /// mid-release deterministically. `(parked, gate)`. Only under cfg(test).
    #[cfg(test)]
    release_gate: StdRwLock<Option<(Arc<Notify>, Arc<Notify>)>>,
}

/// Drop-safety guard for a claimed run.
///
/// While a claimed run future (`prompt`/`prompt_messages`/`continue_run`) is
/// being polled, this guard stays armed. If the caller drops that future
/// before it completes — cancellation, `tokio::spawn` task abort, a
/// `select!` branch, a cleared `FuturesUnordered`, etc. — the guard's `Drop`
/// aborts the run's controller synchronously (so a detached provider stream
/// observes cancellation immediately, even if the owning runtime is shutting
/// down) and then runs the release cleanup on a dedicated thread that builds
/// its own minimal current-thread Tokio runtime. That makes the cleanup
/// independent of the runtime the run future was polled on: it completes even
/// if that runtime has already shut down, and it cannot deadlock a
/// single-thread runtime because it runs on a separate OS thread. This
/// guarantees the agent can never be wedged with `is_streaming`/`active` left
/// set by a dropped run future, including across runtime shutdown/recreation.
///
/// The guard owns a clone of the run's `AbortController` and its unique
/// `generation`. The cleanup only clears state/removes `active` if the
/// `active` slot still carries the same `generation`, so a stale cleanup can
/// never abort/clear a newer claim that has replaced this run. On the normal
/// completion path `execute_claimed` disarms the guard once `release_run` has
/// finished, so the guard's `Drop` is a no-op and there is no double release.
struct ClaimGuard {
    inner: Arc<Inner>,
    controller: AbortController,
    generation: u64,
    disarmed: bool,
}

impl ClaimGuard {
    fn new(inner: Arc<Inner>, controller: AbortController, generation: u64) -> Self {
        Self {
            inner,
            controller,
            generation,
            disarmed: false,
        }
    }

    /// Mark this guard as no longer responsible for cleanup. Used on the
    /// normal completion path once `release_run` has finished, so the guard's
    /// `Drop` does not double-release.
    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        // Abort synchronously, before launching the cleanup, so the provider
        // stream observes cancellation immediately even if the owning runtime
        // is shutting down and the cleanup thread takes a moment to start.
        // `CancellationToken::cancel` is idempotent, so this is safe alongside
        // the cleanup's own abort.
        self.controller.abort();
        let inner = self.inner.clone();
        let generation = self.generation;
        // Run the release cleanup on a dedicated thread with its own
        // minimal current-thread Tokio runtime. This is the exceptional
        // cancellation path: we cannot `await` from `Drop`, and we must not
        // depend on the run's original runtime (it may already be shutting
        // down, and Tokio does not guarantee spawned shutdown tasks complete).
        // A fresh runtime on a separate thread completes the cleanup
        // independently and cannot deadlock a single-thread runtime. The
        // thread is fire-and-forget: it is not joined and does not block the
        // dropping thread.
        std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                // Release the claim only if it still belongs to this run. A
                // newer claim that has replaced this run owns its own release;
                // touching it would abort/clear the wrong run. State is cleared
                // before `active` is removed, and the whole step runs under the
                // `active` lock so a concurrent `claim_run` cannot interleave
                // (it blocks on `active.lock`) — i.e. a new claim is prevented
                // until the old state cleanup completes. Lock order
                // active->state matches `claim_run`.
                release_claim_owned(&inner, generation).await;
            });
        });
    }
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

pub struct Subscription {
    inner: Arc<Inner>,
    index: usize,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(listener) = self.inner.listeners.write().get_mut(self.index) {
            *listener = None;
        }
    }
}

impl Agent {
    #[must_use]
    pub fn new(options: AgentOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: RwLock::new(options.initial_state.clone()),
                listeners: StdRwLock::new(Vec::new()),
                steering: Mutex::new(PendingQueue::new(options.steering_mode)),
                follow_up: Mutex::new(PendingQueue::new(options.follow_up_mode)),
                active: Mutex::new(None),
                run_generation: AtomicU64::new(0),
                idle: Notify::new(),
                stream_options: RwLock::new(options.stream_options.clone()),
                before_agent_start: StdRwLock::new(options.before_agent_start.clone()),
                transform_message: StdRwLock::new(options.transform_message.clone()),
                transform_context: StdRwLock::new(options.transform_context.clone()),
                before_tool_call: StdRwLock::new(options.before_tool_call.clone()),
                after_tool_call: StdRwLock::new(options.after_tool_call.clone()),
                options,
                #[cfg(test)]
                release_gate: StdRwLock::new(None),
            }),
        }
    }

    pub async fn state(&self) -> AgentState {
        self.inner.state.read().await.clone()
    }

    /// Returns the provider options snapshot used for the next model turn.
    pub async fn stream_options(&self) -> SimpleStreamOptions {
        self.inner.stream_options.read().await.clone()
    }

    /// Atomically replaces provider options for subsequent model turns.
    pub async fn set_stream_options(&self, options: SimpleStreamOptions) {
        *self.inner.stream_options.write().await = options;
    }

    /// Returns the provider session identifier used for the next model turn.
    pub async fn session_id(&self) -> Option<String> {
        self.inner.stream_options.read().await.stream.session_id.clone()
    }

    pub async fn set_session_id(&self, session_id: Option<String>) {
        self.inner.stream_options.write().await.stream.session_id = session_id;
    }

    /// Returns per-level thinking budgets used for the next model turn.
    pub async fn thinking_budgets(&self) -> Option<ThinkingBudgets> {
        self.inner.stream_options.read().await.thinking_budgets.clone()
    }

    pub async fn set_thinking_budgets(&self, budgets: Option<ThinkingBudgets>) {
        self.inner.stream_options.write().await.thinking_budgets = budgets;
    }

    /// Returns the preferred provider transport used for the next model turn.
    pub async fn transport(&self) -> Transport {
        self.inner.stream_options.read().await.stream.transport
    }

    pub async fn set_transport(&self, transport: Transport) {
        self.inner.stream_options.write().await.stream.transport = transport;
    }

    /// Returns the cap for provider-requested retry delays, in milliseconds.
    pub async fn max_retry_delay(&self) -> Option<u64> {
        self.inner
            .stream_options
            .read()
            .await
            .stream
            .max_retry_delay_ms
    }

    pub async fn set_max_retry_delay(&self, max_retry_delay_ms: Option<u64>) {
        self.inner
            .stream_options
            .write()
            .await
            .stream
            .max_retry_delay_ms = max_retry_delay_ms;
    }

    /// Returns the synchronous payload hook used for the next model turn.
    pub async fn on_payload(&self) -> Option<PayloadHook> {
        self.inner.stream_options.read().await.stream.on_payload.clone()
    }

    pub async fn set_on_payload(&self, hook: Option<PayloadHook>) {
        self.inner.stream_options.write().await.stream.on_payload = hook;
    }

    /// Returns the synchronous response hook used for the next model turn.
    pub async fn on_response(&self) -> Option<ResponseHook> {
        self.inner.stream_options.read().await.stream.on_response.clone()
    }

    pub async fn set_on_response(&self, hook: Option<ResponseHook>) {
        self.inner.stream_options.write().await.stream.on_response = hook;
    }

    /// Returns the asynchronous request hook used for the next model turn.
    pub async fn before_provider_request(&self) -> Option<BeforeProviderRequestHook> {
        self.inner
            .stream_options
            .read()
            .await
            .stream
            .before_provider_request
            .clone()
    }

    pub async fn set_before_provider_request(&self, hook: Option<BeforeProviderRequestHook>) {
        self.inner
            .stream_options
            .write()
            .await
            .stream
            .before_provider_request = hook;
    }

    /// Returns the asynchronous header hook used for the next model turn.
    pub async fn before_provider_headers(&self) -> Option<BeforeProviderHeadersHook> {
        self.inner
            .stream_options
            .read()
            .await
            .stream
            .before_provider_headers
            .clone()
    }

    pub async fn set_before_provider_headers(&self, hook: Option<BeforeProviderHeadersHook>) {
        self.inner
            .stream_options
            .write()
            .await
            .stream
            .before_provider_headers = hook;
    }

    /// Returns the asynchronous response hook used for the next model turn.
    pub async fn after_provider_response(&self) -> Option<AfterProviderResponseHook> {
        self.inner
            .stream_options
            .read()
            .await
            .stream
            .after_provider_response
            .clone()
    }

    pub async fn set_after_provider_response(&self, hook: Option<AfterProviderResponseHook>) {
        self.inner
            .stream_options
            .write()
            .await
            .stream
            .after_provider_response = hook;
    }

    pub fn set_before_agent_start(&self, hook: Option<BeforeAgentStartFn>) {
        *self.inner.before_agent_start.write() = hook;
    }

    pub fn set_transform_message(&self, hook: Option<TransformMessageFn>) {
        *self.inner.transform_message.write() = hook;
    }

    pub fn set_transform_context(&self, hook: Option<TransformContextFn>) {
        *self.inner.transform_context.write() = hook;
    }

    /// Returns the hook currently installed for the next tool call.
    #[must_use]
    pub fn before_tool_call(&self) -> Option<BeforeToolCallFn> {
        self.inner.before_tool_call.read().clone()
    }

    pub fn set_before_tool_call(&self, hook: Option<BeforeToolCallFn>) {
        *self.inner.before_tool_call.write() = hook;
    }

    pub fn set_after_tool_call(&self, hook: Option<AfterToolCallFn>) {
        *self.inner.after_tool_call.write() = hook;
    }

    pub async fn set_system_prompt(&self, prompt: impl Into<String>) {
        self.inner.state.write().await.system_prompt = prompt.into();
    }

    pub async fn set_model(&self, model: Model) {
        self.inner.state.write().await.model = model;
    }

    pub async fn set_thinking_level(&self, level: ThinkingLevel) {
        self.inner.state.write().await.thinking_level = level;
    }

    pub async fn set_tools(&self, tools: Vec<AgentTool>) {
        self.inner.state.write().await.tools = tools;
    }
    pub async fn set_tools_and_system_prompt(
        &self,
        tools: Vec<AgentTool>,
        prompt: impl Into<String>,
    ) {
        let mut state = self.inner.state.write().await;
        state.tools = tools;
        state.system_prompt = prompt.into();
    }

    pub async fn set_messages(&self, messages: Vec<AgentMessage>) {
        self.inner.state.write().await.messages = messages;
    }

    pub async fn clear_error_message(&self) {
        self.inner.state.write().await.error_message = None;
    }

    pub async fn subscribe<F, Fut>(&self, listener: F) -> Subscription
    where
        F: Fn(AgentEvent, AbortSignal) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let listener: Listener = Arc::new(move |event, signal| Box::pin(listener(event, signal)));
        let mut listeners = self.inner.listeners.write();
        let index = listeners.len();
        listeners.push(Some(listener));
        Subscription {
            inner: self.inner.clone(),
            index,
        }
    }

    pub async fn subscribe_simple<F, Fut>(&self, listener: F) -> Subscription
    where
        F: Fn(AgentEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.subscribe(move |event, _signal| listener(event)).await
    }

    pub async fn steering_mode(&self) -> QueueMode {
        self.inner.steering.lock().await.mode()
    }

    pub async fn set_steering_mode(&self, mode: QueueMode) {
        self.inner.steering.lock().await.set_mode(mode);
    }

    pub async fn follow_up_mode(&self) -> QueueMode {
        self.inner.follow_up.lock().await.mode()
    }

    pub async fn set_follow_up_mode(&self, mode: QueueMode) {
        self.inner.follow_up.lock().await.set_mode(mode);
    }

    pub async fn pending_message_count(&self) -> usize {
        let steering = self.inner.steering.lock().await;
        let follow_up = self.inner.follow_up.lock().await;
        steering.len() + follow_up.len()
    }
    pub async fn queued_messages(&self) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        let steering = self.inner.steering.lock().await;
        let follow_up = self.inner.follow_up.lock().await;
        (steering.snapshot(), follow_up.snapshot())
    }

    pub async fn drain_queued_messages(&self) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        let mut steering = self.inner.steering.lock().await;
        let mut follow_up = self.inner.follow_up.lock().await;
        (steering.drain_all(), follow_up.drain_all())
    }

    pub async fn steer(&self, message: AgentMessage) {
        self.inner.steering.lock().await.enqueue(message);
    }

    pub async fn follow_up(&self, message: AgentMessage) {
        self.inner.follow_up.lock().await.enqueue(message);
    }

    pub async fn clear_steering_queue(&self) {
        self.inner.steering.lock().await.clear();
    }

    pub async fn clear_follow_up_queue(&self) {
        self.inner.follow_up.lock().await.clear();
    }

    pub async fn clear_all_queues(&self) {
        self.clear_steering_queue().await;
        self.clear_follow_up_queue().await;
    }

    pub async fn has_queued_messages(&self) -> bool {
        self.inner.steering.lock().await.has_items()
            || self.inner.follow_up.lock().await.has_items()
    }

    pub async fn abort(&self) {
        if let Some(active) = self.inner.active.lock().await.as_ref() {
            active.controller.abort();
        }
    }

    pub async fn wait_for_idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            if self.inner.active.lock().await.is_none() {
                return;
            }
            notified.await;
        }
    }

    pub async fn reset(&self) {
        let mut state = self.inner.state.write().await;
        state.messages.clear();
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.error_message = None;
        drop(state);
        self.clear_all_queues().await;
    }

    pub async fn prompt(&self, text: impl Into<String>) -> Result<()> {
        let user = UserMessage {
            content: vec![ContentBlock::text(text.into())],
            timestamp: now_millis(),
        };
        self.prompt_messages(vec![Message::User(user)]).await
    }

    pub async fn prompt_messages(&self, messages: Vec<AgentMessage>) -> Result<()> {
        let (signal, guard) = self.claim_run().await?;
        let mut context = self.context_snapshot().await;
        let messages = self.prepare_agent_start(messages, &mut context).await?;
        let config = self.loop_config().await;
        let emit = self.event_sink();
        let stream_fn = self.inner.options.stream_fn.clone();
        self.execute_claimed(guard, signal.clone(), async move {
            run_agent_loop(messages, context, config, emit, stream_fn, signal).await
        })
        .await
    }

    pub async fn continue_run(&self) -> Result<()> {
        let state = self.state().await;
        let Some(last) = state.messages.last() else {
            return Err(anyhow!("No messages to continue from"));
        };
        if matches!(last, Message::Assistant(_)) {
            let (signal, mut guard) = self.claim_run().await?;
            let mut drained = self.inner.steering.lock().await.drain();
            let drained_steering = !drained.is_empty();
            if drained.is_empty() {
                drained = self.inner.follow_up.lock().await.drain();
            }
            if drained.is_empty() {
                // Nothing to continue with: release the claim inline. The guard
                // stays armed during the awaited release so that cancellation
                // while `release_run` is blocked still triggers the guard's
                // cleanup; only after the release has completed do we disarm so
                // `Drop` does not double-release.
                let generation = guard.generation;
                self.release_run(generation).await;
                guard.disarm();
                return Err(anyhow!("Cannot continue from message role: assistant"));
            }
            let mut context = self.context_snapshot().await;
            let drained = self.prepare_agent_start(drained, &mut context).await?;
            let mut config = self.loop_config().await;
            config.skip_initial_steering = drained_steering;
            let emit = self.event_sink();
            let stream_fn = self.inner.options.stream_fn.clone();
            return self
                .execute_claimed(guard, signal.clone(), async move {
                    run_agent_loop(drained, context, config, emit, stream_fn, signal).await
                })
                .await;
        }

        let (signal, guard) = self.claim_run().await?;
        let mut context = self.context_snapshot().await;
        let initial = self.prepare_agent_start(Vec::new(), &mut context).await?;
        context.messages.extend(initial);
        let config = self.loop_config().await;
        let emit = self.event_sink();
        let stream_fn = self.inner.options.stream_fn.clone();
        self.execute_claimed(guard, signal.clone(), async move {
            run_agent_loop_continue(context, config, emit, stream_fn, signal).await
        })
        .await
    }

    pub async fn r#continue(&self) -> Result<()> {
        self.continue_run().await
    }

    pub async fn wait(&self) {
        self.wait_for_idle().await;
    }

    async fn claim_run(&self) -> Result<(AbortSignal, ClaimGuard)> {
        let mut active = self.inner.active.lock().await;
        if active.is_some() {
            return Err(anyhow!(
                "Agent is already processing. Use steer() or follow_up() to queue messages, or wait for completion."
            ));
        }
        let (controller, signal) = AbortController::new();
        // Mint a fresh, unique generation for this run so a stale drop cleanup
        // can verify ownership before releasing.
        let generation = self.inner.run_generation.fetch_add(1, Ordering::SeqCst) + 1;
        *active = Some(ActiveRun {
            controller: controller.clone(),
            signal: signal.clone(),
            generation,
            terminal_failure_finalized: false,
        });
        // Arm the drop-safety guard as soon as the claim is taken, before any
        // further await, so a drop between `claim_run` and `execute_claimed`
        // (e.g. while snapshotting context) still releases the claim. The guard
        // owns a controller clone so it can abort synchronously from `Drop`.
        let guard = ClaimGuard::new(self.inner.clone(), controller.clone(), generation);
        // Hold `active` across the initial state write so the claim setup
        // (active slot + streaming flags) is atomic with respect to any
        // concurrent release: a release/cleanup serializes on `active.lock`
        // and cannot observe `active == Some` with `is_streaming` still false
        // or interleave its state clear with this state write. Lock order
        // active->state matches `release_claim_owned`.
        let mut state = self.inner.state.write().await;
        state.is_streaming = true;
        state.streaming_message = None;
        state.error_message = None;
        drop(state);
        drop(active);
        Ok((signal, guard))
    }

    async fn release_run(&self, generation: u64) {
        release_claim_owned(&self.inner, generation).await;
    }

    async fn execute_claimed<F>(
        &self,
        guard: ClaimGuard,
        signal: AbortSignal,
        execution: F,
    ) -> Result<()>
    where
        F: Future<Output = Result<Vec<AgentMessage>>> + Send,
    {
        // `guard` is armed for the lifetime of this future. If the caller drops
        // it before completion (cancellation, `tokio::spawn` task abort, a
        // `select!` branch, a cleared `FuturesUnordered`, etc.) the guard's
        // `Drop` aborts the run's controller synchronously and schedules a
        // cleanup that releases the claim, so the agent can never be wedged. On
        // the normal path the guard is disarmed below once `release_run` has
        // finished.
        let generation = guard.generation;
        let mut guard = Some(guard);
        let result = match AssertUnwindSafe(execution).catch_unwind().await {
            Ok(result) => result,
            Err(panic) => Err(anyhow!(panic_message(panic))),
        };
        let outcome = if let Err(error) = result {
            if self.terminal_failure_finalized().await {
                self.release_run(generation).await;
                Err(error)
            } else {
                let _secondary_error = self
                    .emit_failure_turn(error.to_string(), signal.is_aborted())
                    .await
                    .err();
                self.release_run(generation).await;
                Err(error)
            }
        } else {
            self.release_run(generation).await;
            Ok(())
        };
        // Normal completion: disarm so the guard's `Drop` does not
        // double-release. `release_claim_owned` is idempotent regardless (it
        // only acts on a matching `generation`), but disarming keeps the normal
        // path to a single release.
        if let Some(mut guard) = guard.take() {
            guard.disarm();
        }
        outcome
    }

    async fn terminal_failure_finalized(&self) -> bool {
        self.inner
            .active
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| active.terminal_failure_finalized)
    }

    async fn context_snapshot(&self) -> AgentContext {
        let state = self.inner.state.read().await;
        AgentContext {
            system_prompt: state.system_prompt.clone(),
            messages: state.messages.clone(),
            tools: state.tools.clone(),
        }
    }

    async fn prepare_agent_start(
        &self,
        messages: Vec<AgentMessage>,
        context: &mut AgentContext,
    ) -> Result<Vec<AgentMessage>> {
        let Some(hook) = self.inner.before_agent_start.read().clone() else {
            return Ok(messages);
        };
        let result = hook(BeforeAgentStartContext {
            system_prompt: context.system_prompt.clone(),
            messages,
        })
        .await?;
        context.system_prompt = result.system_prompt.clone();
        self.inner.state.write().await.system_prompt = result.system_prompt;
        Ok(result.messages)
    }

    async fn loop_config(&self) -> AgentLoopConfig {
        let state = self.inner.state.read().await;
        let steering_inner = self.inner.clone();
        let follow_up_inner = self.inner.clone();
        AgentLoopConfig {
            model: state.model.clone(),
            reasoning: state.thinking_level,
            stream_options: self.inner.stream_options.read().await.clone(),
            tool_execution: self.inner.options.tool_execution,
            convert_to_llm: self.inner.options.convert_to_llm.clone(),
            transform_context: self.inner.transform_context.read().clone(),
            get_api_key: self.inner.options.get_api_key.clone(),
            before_tool_call: self.inner.before_tool_call.read().clone(),
            after_tool_call: self.inner.after_tool_call.read().clone(),
            prepare_next_turn: self.inner.options.prepare_next_turn.clone(),
            get_steering_messages: Some(Arc::new(move || {
                let inner = steering_inner.clone();
                Box::pin(async move { inner.steering.lock().await.drain() })
            })),
            get_follow_up_messages: Some(Arc::new(move || {
                let inner = follow_up_inner.clone();
                Box::pin(async move { inner.follow_up.lock().await.drain() })
            })),
            ..AgentLoopConfig::default()
        }
    }

    fn event_sink(&self) -> crate::EventSink {
        let agent = self.clone();
        Arc::new(move |event| {
            let agent = agent.clone();
            Box::pin(async move { agent.process_event(event).await })
        })
    }

    async fn transform_event(&self, event: AgentEvent, signal: AbortSignal) -> Result<AgentEvent> {
        let Some(transform) = self.inner.transform_message.read().clone() else {
            return Ok(event);
        };
        let AgentEvent::MessageEnd { message } = event else {
            return Ok(event);
        };
        let original_role = message.role();
        let transformed = transform(message, signal).await?;
        if transformed.role() != original_role {
            return Err(anyhow!(
                "message_end replacement must preserve the original message role"
            ));
        }
        Ok(AgentEvent::MessageEnd { message: transformed })
    }

    async fn process_event(&self, event: AgentEvent) -> Result<()> {
        let signal = self
            .inner
            .active
            .lock()
            .await
            .as_ref()
            .map_or_else(|| AbortController::new().1, |active| active.signal.clone());
        let event = self.transform_event(event, signal.clone()).await?;
        let terminal_failure_finalized = matches!(
            &event,
            AgentEvent::AgentEnd { messages }
                if matches!(
                    messages.last(),
                    Some(Message::Assistant(message))
                        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
                )
        );

        {
            let mut state = self.inner.state.write().await;
            match &event {
                AgentEvent::MessageStart { message }
                | AgentEvent::MessageUpdate { message, .. } => {
                    state.streaming_message = Some(message.clone());
                }
                AgentEvent::MessageEnd { message } => {
                    state.streaming_message = None;
                    state.messages.push(message.clone());
                }
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    state.pending_tool_calls.insert(tool_call_id.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    state.pending_tool_calls.remove(tool_call_id);
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if let Some(error_message) = &message.error_message {
                        state.error_message = Some(error_message.clone());
                    }
                }
                AgentEvent::AgentEnd { .. } => state.streaming_message = None,
                AgentEvent::AgentStart
                | AgentEvent::TurnStart
                | AgentEvent::ToolExecutionUpdate { .. } => {}
            }
        }

        if terminal_failure_finalized {
            if let Some(active) = self.inner.active.lock().await.as_mut() {
                active.terminal_failure_finalized = true;
            }
        }

        let listeners = self.inner.listeners.read().clone();
        let mut first_error = None;
        for listener in listeners.into_iter().flatten() {
            let listener_event = event.clone();
            let listener_signal = signal.clone();
            let outcome =
                AssertUnwindSafe(async move { listener(listener_event, listener_signal).await })
                    .catch_unwind()
                    .await;
            let listener_result: Result<()> = match outcome {
                Ok(inner) => inner,
                Err(panic) => Err(anyhow!(panic_message(panic))),
            };
            if let Err(error) = listener_result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    #[cfg(test)]
    pub(crate) async fn hold_state_read_for_test(
        &self,
        acquired: Arc<Notify>,
        release: Arc<Notify>,
    ) {
        let _state = self.inner.state.read().await;
        acquired.notify_one();
        release.notified().await;
    }

    #[cfg(test)]
    pub(crate) async fn claim_run_for_test(&self) -> Result<u64> {
        // Claim a run without driving the loop, disarming the guard so the
        // `active` claim stays set for the test to exercise
        // `release_claim_owned`'s generation guard directly.
        let (_signal, mut guard) = self.claim_run().await?;
        let generation = guard.generation;
        guard.disarm();
        Ok(generation)
    }

    #[cfg(test)]
    pub(crate) async fn current_run_generation_for_test(&self) -> Option<u64> {
        self.inner
            .active
            .lock()
            .await
            .as_ref()
            .map(|active| active.generation)
    }

    #[cfg(test)]
    pub(crate) fn set_release_gate_for_test(&self, gate: Option<(Arc<Notify>, Arc<Notify>)>) {
        *self.inner.release_gate.write() = gate;
    }

    #[cfg(test)]
    pub(crate) async fn release_run_generation_for_test(&self, generation: u64) {
        release_claim_owned(&self.inner, generation).await;
    }

    async fn emit_failure_turn(&self, error_message: String, aborted: bool) -> Result<()> {
        let model = self.inner.state.read().await.model.clone();
        let failure = AssistantMessage {
            content: vec![ContentBlock::text("")],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id,
            response_model: None,
            response_id: None,
            diagnostics: Vec::new(),
            usage: Usage::default(),
            stop_reason: if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            },
            error_message: Some(error_message),
            raw_stop_reason: None,
            timestamp: now_millis(),
        };
        let mut first_error = None;
        for event in [
            AgentEvent::MessageStart {
                message: Message::Assistant(failure.clone()),
            },
            AgentEvent::MessageEnd {
                message: Message::Assistant(failure.clone()),
            },
            AgentEvent::TurnEnd {
                message: failure.clone(),
                tool_results: Vec::new(),
            },
            AgentEvent::AgentEnd {
                messages: vec![Message::Assistant(failure)],
            },
        ] {
            if let Err(error) = self.process_event(event).await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod before_tool_call_getter_tests {
    use super::*;
    use crate::BeforeToolCallResult;

    #[test]
    fn getter_returns_current_hook_and_tracks_replacement() {
        let agent = Agent::new(AgentOptions::default());
        assert!(agent.before_tool_call().is_none());
        let hook: BeforeToolCallFn = Arc::new(|_| {
            Box::pin(async { Ok(BeforeToolCallResult::default()) })
        });
        agent.set_before_tool_call(Some(hook.clone()));
        let current = agent.before_tool_call().expect("current hook");
        assert!(Arc::ptr_eq(&current, &hook));
        agent.set_before_tool_call(None);
        assert!(agent.before_tool_call().is_none());
    }
}

/// Publish non-streaming state, remove the active claim, and notify idle,
/// but only if the `active` slot still carries `generation`.
///
/// Shared by `release_run` (normal completion/error path) and the
/// `ClaimGuard` cleanup (drop/cancellation path). The `generation` check makes
/// a stale drop cleanup a no-op if a newer claim has replaced this run, so it
/// can never abort/clear the wrong run. The whole step runs under the
/// `active` lock: a concurrent `claim_run` blocks on `active.lock` until this
/// release finishes (state cleared and `active` removed), so a new claim is
/// prevented until the old state cleanup is complete. Lock order
/// active->state matches `claim_run`. State is cleared before `active` is
/// removed, preserving the invariant that any observer seeing
/// `active == None` also sees non-streaming state.
async fn release_claim_owned(inner: &Arc<Inner>, generation: u64) {
    let mut active = inner.active.lock().await;
    let owned = active
        .as_ref()
        .is_some_and(|active| active.generation == generation);
    if !owned {
        return;
    }
    // Test seam: signal that the release is in flight (`parked`), then park
    // until the test opens `gate`. Lets tests synchronously know a release is
    // in flight and cancel it mid-release deterministically.
    #[cfg(test)]
    {
        let seam = inner.release_gate.read().as_ref().cloned();
        if let Some((parked, gate)) = seam {
            parked.notify_waiters();
            gate.notified().await;
        }
    }
    {
        let mut state = inner.state.write().await;
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
    }
    let active = active.take().expect("owned run present");
    active.controller.abort();
    inner.idle.notify_waiters();
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "panic in agent run".to_owned()
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
