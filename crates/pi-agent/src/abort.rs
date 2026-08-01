use tokio_util::sync::CancellationToken;

/// A clonable cancellation signal used by agent runs and tool executions.
///
/// Mirrors the web/`AbortSignal` contract used by pi-agent-core: cheap to clone,
/// polled via [`AbortSignal::is_aborted`], and awaitable via
/// [`AbortSignal::cancelled`].
#[derive(Clone, Debug)]
pub struct AbortSignal {
    token: CancellationToken,
}

/// Cancels every [`AbortSignal`] cloned from this controller.
///
/// Equivalent to the DOM `AbortController`: construct a pair, pass the signal
/// into the agent loop / tools, and call [`AbortController::abort`] to cancel.
#[derive(Clone, Debug)]
pub struct AbortController {
    token: CancellationToken,
}

impl AbortController {
    /// Create a fresh controller and its linked signal.
    #[must_use]
    pub fn new() -> (Self, AbortSignal) {
        let token = CancellationToken::new();
        (
            Self {
                token: token.clone(),
            },
            AbortSignal { token },
        )
    }

    /// Wrap an existing [`CancellationToken`] as an abort controller.
    #[must_use]
    pub fn from_token(token: CancellationToken) -> (Self, AbortSignal) {
        (
            Self {
                token: token.clone(),
            },
            AbortSignal { token },
        )
    }

    /// Request cancellation. Idempotent.
    pub fn abort(&self) {
        self.token.cancel();
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Borrow a signal linked to this controller.
    #[must_use]
    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            token: self.token.clone(),
        }
    }

    /// Create a child controller that is aborted when either this controller or
    /// the child itself is aborted. Used for tool-scoped cancellation that must
    /// still honor the parent run signal.
    #[must_use]
    pub fn child(&self) -> (Self, AbortSignal) {
        let token = self.token.child_token();
        (
            Self {
                token: token.clone(),
            },
            AbortSignal { token },
        )
    }
}

impl Default for AbortController {
    fn default() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }
}

impl AbortSignal {
    /// A signal that is never aborted. Useful for standalone tool calls.
    #[must_use]
    pub fn none() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Wrap an existing cancellation token.
    #[must_use]
    pub fn from_token(token: CancellationToken) -> Self {
        Self { token }
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Returns `Err` when the signal has already been aborted.
    pub fn check(&self) -> Result<(), AbortError> {
        if self.is_aborted() {
            Err(AbortError)
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }
}

/// Error returned when an operation observes an aborted [`AbortSignal`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbortError;

impl std::fmt::Display for AbortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Operation aborted")
    }
}

impl std::error::Error for AbortError {}

#[cfg(test)]
mod abort_tests {
    use super::*;

    #[test]
    fn controller_aborts_linked_signals() {
        let (controller, signal) = AbortController::new();
        assert!(!signal.is_aborted());
        assert!(signal.check().is_ok());
        controller.abort();
        assert!(controller.is_aborted());
        assert!(signal.is_aborted());
        assert!(signal.check().is_err());
        assert_eq!(signal.check().unwrap_err().to_string(), "Operation aborted");
    }

    #[test]
    fn child_aborts_with_parent_but_not_siblings() {
        let (parent, parent_signal) = AbortController::new();
        let (child, child_signal) = parent.child();
        let (sibling, sibling_signal) = parent.child();

        child.abort();
        assert!(child_signal.is_aborted());
        assert!(!parent_signal.is_aborted());
        assert!(!sibling_signal.is_aborted());

        parent.abort();
        assert!(parent_signal.is_aborted());
        assert!(sibling_signal.is_aborted());
    }

    #[tokio::test]
    async fn cancelled_resolves_on_abort() {
        let (controller, signal) = AbortController::new();
        let wait = tokio::spawn({
            let signal = signal.clone();
            async move {
                signal.cancelled().await;
            }
        });
        controller.abort();
        wait.await.expect("join");
        assert!(signal.is_aborted());
    }
}
