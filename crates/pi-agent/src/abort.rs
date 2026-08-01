use tokio_util::sync::CancellationToken;

/// A clonable cancellation signal used by agent runs and tool executions.
#[derive(Clone, Debug)]
pub struct AbortSignal {
    token: CancellationToken,
}

/// Cancels every [`AbortSignal`] cloned from this controller.
#[derive(Clone, Debug)]
pub struct AbortController {
    token: CancellationToken,
}

impl AbortController {
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

    pub fn abort(&self) {
        self.token.cancel();
    }

    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    #[must_use]
    pub fn signal(&self) -> AbortSignal {
        AbortSignal {
            token: self.token.clone(),
        }
    }
}

impl AbortSignal {
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }
}
