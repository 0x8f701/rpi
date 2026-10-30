use std::io::Write;
use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use pi_coding::{AuthEvent, AuthInteraction, AuthPrompt, CredentialInfo};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

const MAX_FRAME: usize = 1024 * 1024;

#[cfg(unix)]
struct Transport {
    fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    original_flags: nix::fcntl::OFlag,
    restored: bool,
}
#[cfg(unix)]
impl Transport {
    fn duplicate(source: impl std::os::fd::AsFd) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        let owned = source.as_fd().try_clone_to_owned()?;
        let original_flags = OFlag::from_bits_truncate(fcntl(owned.as_raw_fd(), FcntlArg::F_GETFL)?);
        fcntl(owned.as_raw_fd(), FcntlArg::F_SETFL(original_flags | OFlag::O_NONBLOCK))?;
        match tokio::io::unix::AsyncFd::new(owned) {
            Ok(fd) => Ok(Self { fd, original_flags, restored: false }),
            Err(_) => {
                fcntl(source.as_fd().as_raw_fd(), FcntlArg::F_SETFL(original_flags))
                    .map_err(|_| std::io::Error::other("transport_setup_and_restore_failed"))?;
                Err(std::io::Error::other("transport_setup_failed"))
            }
        }
    }
    fn restore(&mut self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        if !self.restored {
            nix::fcntl::fcntl(self.fd.get_ref().as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(self.original_flags))
                .map_err(|_| std::io::Error::other("transport_restore_failed"))?;
            self.restored = true;
        }
        Ok(())
    }
}
#[cfg(unix)]
impl Drop for Transport {
    fn drop(&mut self) {
        if self.restore().is_err() {
            eprintln!("rpi: transport_restore_failed");
        }
    }
}
#[cfg(unix)]
impl AsyncRead for Transport {
    fn poll_read(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buffer: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        use std::os::fd::AsRawFd;
        loop {
            let mut ready = std::task::ready!(self.fd.poll_read_ready(cx))?;
            match ready.try_io(|fd| nix::unistd::read(fd.get_ref().as_raw_fd(), buffer.initialize_unfilled()).map_err(std::io::Error::from)) {
                Ok(Ok(count)) => { buffer.advance(count); return std::task::Poll::Ready(Ok(())); }
                Ok(Err(error)) => return std::task::Poll::Ready(Err(error)),
                Err(_) => continue,
            }
        }
    }
}
#[cfg(unix)]
impl AsyncWrite for Transport {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, bytes: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        loop {
            let mut ready = std::task::ready!(self.fd.poll_write_ready(cx))?;
            match ready.try_io(|fd| nix::unistd::write(fd.get_ref(), bytes).map_err(std::io::Error::from)) {
                Ok(result) => return std::task::Poll::Ready(result),
                Err(_) => continue,
            }
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Failure { Cancelled, InvalidInput, Io, AuthenticationFailed }
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Command { Response { id: u64, value: String }, Cancel {} }
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame<'a> {
    Prompt { id: u64, prompt: &'a AuthPrompt },
    Event { event: &'a AuthEvent },
    Complete {
        #[serde(rename = "providerId")]
        provider_id: &'a str,
        #[serde(rename = "credentialType")]
        credential_type: &'a str,
    },
    Error { code: Failure },
}
struct CappedFrame(Vec<u8>);
impl Write for CappedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_FRAME - self.0.len() { return Err(std::io::Error::other("frame exceeds limit")); }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
fn serialize(frame: &Frame<'_>) -> std::io::Result<Vec<u8>> {
    let mut output = CappedFrame(Vec::new());
    serde_json::to_writer(&mut output, frame).map_err(std::io::Error::other)?;
    output.0.push(b'\n');
    Ok(output.0)
}
struct Pending { id: u64, choices: Option<Vec<String>>, response: oneshot::Sender<String> }
struct State { pending: Option<Pending>, next_id: u64, failure: Option<Failure>, terminal: bool }
struct JsonInteraction {
    state: Mutex<State>,
    failure: watch::Sender<Option<Failure>>,
    output: mpsc::Sender<Vec<u8>>,
}
impl JsonInteraction {
    fn new() -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (failure, _) = watch::channel(None);
        let (output, receiver) = mpsc::channel(8);
        (Self { state: Mutex::new(State { pending: None, next_id: 1, failure: None, terminal: false }), failure, output }, receiver)
    }
    fn fail_locked(&self, state: &mut State, failure: Failure) {
        if state.failure.is_none() && !state.terminal {
            state.failure = Some(failure);
            state.pending = None;
            self.failure.send_replace(Some(failure));
        }
    }
    fn fail(&self, failure: Failure) { self.fail_locked(&mut self.state.lock(), failure); }
    fn emit(&self, state: &mut State, frame: &Frame<'_>) {
        if serialize(frame).ok().and_then(|bytes| self.output.try_send(bytes).ok()).is_none() {
            self.fail_locked(state, Failure::Io);
        }
    }
    fn command(&self, bytes: &[u8]) {
        let mut state = self.state.lock();
        if state.terminal || state.failure.is_some() { return; }
        match serde_json::from_slice::<Command>(bytes) {
            Ok(Command::Cancel {}) => self.fail_locked(&mut state, Failure::Cancelled),
            Ok(Command::Response { id, value }) => {
                if !state.pending.as_ref().is_some_and(|pending| pending.id == id && pending.choices.as_ref().is_none_or(|choices| choices.contains(&value))) {
                    self.fail_locked(&mut state, Failure::InvalidInput);
                } else if let Some(pending) = state.pending.take() && pending.response.send(value).is_err() {
                    self.fail_locked(&mut state, Failure::InvalidInput);
                }
            }
            Err(_) => self.fail_locked(&mut state, Failure::InvalidInput),
        }
    }
    async fn read(&self, mut reader: impl AsyncRead + Unpin) {
        let mut chunk = [0_u8; 8192];
        let mut frame = Vec::new();
        loop {
            let count = match reader.read(&mut chunk).await {
                Ok(0) => { self.fail(if frame.is_empty() { Failure::Cancelled } else { Failure::InvalidInput }); return; }
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => { self.fail(Failure::Io); return; }
            };
            for byte in &chunk[..count] {
                if *byte == b'\n' {
                    self.command(&frame);
                    frame.clear();
                    if self.state.lock().failure.is_some() { return; }
                } else {
                    if frame.len() == MAX_FRAME { self.fail(Failure::InvalidInput); return; }
                    frame.push(*byte);
                }
            }
        }
    }
}
#[async_trait]
impl AuthInteraction for JsonInteraction {
    async fn prompt(&self, prompt: AuthPrompt) -> Result<String> {
        let receive = {
            let mut state = self.state.lock();
            if state.failure.is_some() || state.terminal || state.pending.is_some() {
                self.fail_locked(&mut state, Failure::InvalidInput);
                anyhow::bail!("authentication interaction unavailable");
            }
            let id = state.next_id;
            let Some(next) = id.checked_add(1) else { self.fail_locked(&mut state, Failure::InvalidInput); anyhow::bail!("authentication interaction unavailable"); };
            state.next_id = next;
            let (send, receive) = oneshot::channel();
            let choices = match &prompt { AuthPrompt::Select { options, .. } => Some(options.iter().map(|option| option.id.clone()).collect()), _ => None };
            state.pending = Some(Pending { id, choices, response: send });
            self.emit(&mut state, &Frame::Prompt { id, prompt: &prompt });
            receive
        };
        receive.await.map_err(|_| anyhow::anyhow!("authentication interaction unavailable"))
    }
    fn notify(&self, event: AuthEvent) {
        let mut state = self.state.lock();
        if state.failure.is_none() && !state.terminal { self.emit(&mut state, &Frame::Event { event: &event }); }
    }
}

async fn drive(
    interaction: &JsonInteraction,
    mut output: mpsc::Receiver<Vec<u8>>,
    input: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    login: impl Future<Output = Result<CredentialInfo>>,
) -> bool {
    let reader = interaction.read(input);
    tokio::pin!(reader, login);
    let mut failure = interaction.failure.subscribe();
    let mut current: Option<(Vec<u8>, usize)> = None;
    let mut result: Option<Result<CredentialInfo>> = None;
    let mut terminal = false;
    loop {
        if let Some(code) = *failure.borrow_and_update() {
            // A partially delivered frame cannot be followed by another JSON frame.
            if current.as_ref().is_none_or(|(_, offset)| *offset == 0) {
                if let Ok(bytes) = serialize(&Frame::Error { code }) {
                    let _ = tokio::time::timeout(std::time::Duration::from_millis(250), writer.write_all(&bytes)).await;
                }
            }
            return false;
        }
        if current.is_none() {
            if let Ok(bytes) = output.try_recv() { current = Some((bytes, 0)); }
            else if let Some(outcome) = result.take() {
                let frame = match &outcome {
                    Ok(info) => Frame::Complete { provider_id: &info.provider_id, credential_type: info.credential_type.label() },
                    Err(_) => Frame::Error { code: Failure::AuthenticationFailed },
                };
                let Ok(bytes) = serialize(&frame) else { return false; };
                terminal = true;
                result = Some(outcome);
                current = Some((bytes, 0));
            }
        }
        tokio::select! {
            biased;
            () = &mut reader => {},
            _ = failure.changed() => {},
            written = async {
                let (bytes, offset) = current.as_ref().expect("guarded output");
                writer.write(&bytes[*offset..]).await
            }, if current.is_some() => {
                match written {
                    Ok(0) | Err(_) => { interaction.fail(Failure::Io); }
                    Ok(count) => {
                        let (bytes, offset) = current.as_mut().expect("active output");
                        *offset += count;
                        if *offset == bytes.len() {
                            current = None;
                            if terminal { interaction.state.lock().terminal = true; return result.is_some_and(|value| value.is_ok()); }
                        }
                    }
                }
            }
            outcome = &mut login, if result.is_none() && !terminal => {
                result = Some(outcome);
                if interaction.state.lock().pending.is_some() { interaction.fail(Failure::InvalidInput); }
            }
            frame = output.recv(), if current.is_none() && result.is_none() => {
                if let Some(bytes) = frame { current = Some((bytes, 0)); }
            }
        }
    }
}

pub(crate) async fn login(provider: Option<&str>, scope: Option<&str>) -> Result<()> {
    let (interaction, output) = JsonInteraction::new();
    #[cfg(unix)]
    let transport = match Transport::duplicate(std::io::stdin()) {
        Ok(mut input) => match Transport::duplicate(std::io::stdout()) {
            Ok(output) => Ok((input, output)),
            Err(error) => match input.restore() {
                Ok(()) => Err(error),
                Err(_) => Err(std::io::Error::other("transport_setup_and_restore_failed")),
            },
        },
        Err(error) => Err(error),
    };
    #[cfg(not(unix))]
    let transport: std::io::Result<(tokio::io::Empty, tokio::io::Sink)> = Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "unsupported JSON login transport"));
    let success = match transport {
        Ok((mut input, mut writer)) => {
            let success = drive(&interaction, output, &mut input, &mut writer, async {
                crate::auth_commands::manager()?.login(provider, None, scope, &interaction).await
            }).await;
            #[cfg(unix)]
            {
                let input_restored = input.restore();
                let output_restored = writer.restore();
                if input_restored.is_err() || output_restored.is_err() {
                    anyhow::bail!("transport_restore_failed");
                }
            }
            success
        }
        Err(_) => false,
    };
    if success { Ok(()) } else { anyhow::bail!("authentication_failed") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_coding::{AuthPromptOption, AuthType};

    fn credential() -> CredentialInfo {
        CredentialInfo { provider_id: "synthetic-provider".into(), credential_type: AuthType::ApiKey, scope: None }
    }

    #[tokio::test]
    async fn strict_commands_and_bounded_eof() {
        for bytes in [
            b"{\"type\":\"cancel\",\"extra\":true}\n".as_slice(),
            b"{\"type\":[\"cancel\"]}\n",
            b"{\"type\":\"response\",\"id\":1,\"value\":\"SYNTHETIC_LOCAL_VALUE\"}\n",
            b"{\"type\":\"cancel\"}", &[0xff, b'\n'],
        ] {
            let (interaction, _output) = JsonInteraction::new();
            interaction.read(bytes).await;
            assert_eq!(interaction.state.lock().failure, Some(Failure::InvalidInput), "input {bytes:?}");
        }
        let (interaction, _output) = JsonInteraction::new();
        interaction.read(vec![b' '; MAX_FRAME + 1].as_slice()).await;
        assert_eq!(interaction.state.lock().failure, Some(Failure::InvalidInput));
    }

    #[tokio::test]
    async fn prompts_validate_ids_and_exact_choices() {
        for response in [
            r#"{"type":"response","id":2,"value":"synthetic-choice"}"#,
            r#"{"type":"response","id":1,"value":"1"}"#,
            r#"{"type":"response","id":"1","value":"synthetic-choice"}"#,
        ] {
            let (interaction, _output) = JsonInteraction::new();
            let prompt = interaction.prompt(AuthPrompt::Select { message: "Synthetic choice".into(), options: vec![AuthPromptOption { id: "synthetic-choice".into(), label: "Synthetic".into(), description: None }] });
            tokio::pin!(prompt);
            assert!(futures_util::poll!(&mut prompt).is_pending());
            interaction.command(response.as_bytes());
            assert!(prompt.await.is_err());
            assert_eq!(interaction.state.lock().failure, Some(Failure::InvalidInput));
        }
        let (interaction, mut output) = JsonInteraction::new();
        for id in 1..=2 {
            let prompt = interaction.prompt(AuthPrompt::Secret { message: "Synthetic secret".into(), placeholder: None });
            tokio::pin!(prompt);
            assert!(futures_util::poll!(&mut prompt).is_pending());
            let bytes = output.recv().await.unwrap();
            let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(frame["id"], id);
            interaction.command(format!(r#"{{"type":"response","id":{id},"value":"SYNTHETIC_LOCAL_VALUE"}}"#).as_bytes());
            assert_eq!(prompt.await.unwrap(), "SYNTHETIC_LOCAL_VALUE");
            assert!(!String::from_utf8(bytes).unwrap().contains("SYNTHETIC_LOCAL_VALUE"));
        }
    }

    #[tokio::test]
    async fn oversized_notification_is_capped_and_forbids_complete() {
        let mut capped = CappedFrame(Vec::new());
        assert!(capped.write_all(&vec![b'x'; MAX_FRAME + 1]).is_err());
        assert!(capped.0.is_empty());
        let (interaction, output) = JsonInteraction::new();
        let (input, _parent) = tokio::io::duplex(32);
        let mut bytes = Vec::new();
        assert!(!drive(&interaction, output, input, &mut bytes, async {
            interaction.notify(AuthEvent::Progress { message: "x".repeat(MAX_FRAME) });
            Ok(credential())
        }).await);
        assert_eq!(bytes, b"{\"type\":\"error\",\"code\":\"io\"}\n");
    }

    #[tokio::test]
    async fn notify_delivery_failure_and_provider_errors_are_safe() {
        for broken_writer in [false, true] {
            let (interaction, output) = JsonInteraction::new();
            let (input, _parent) = tokio::io::duplex(32);
            let (writer, remote) = tokio::io::duplex(32);
            if broken_writer { drop(remote); }
            let mut bytes = Vec::new();
            if broken_writer {
                assert!(!drive(&interaction, output, input, writer, async {
                    interaction.notify(AuthEvent::Progress { message: "Synthetic event".into() });
                    Ok(credential())
                }).await);
                assert_eq!(interaction.state.lock().failure, Some(Failure::Io));
            } else {
                assert!(!drive(&interaction, output, input, &mut bytes, async { anyhow::bail!("SYNTHETIC_SECRET_PROVIDER_ERROR") }).await);
                assert_eq!(bytes, b"{\"type\":\"error\",\"code\":\"authentication_failed\"}\n");
            }
        }
    }

    #[tokio::test]
    async fn completion_waits_for_login_and_queued_events() {
        let (interaction, output) = JsonInteraction::new();
        let (input, _parent) = tokio::io::duplex(32);
        let mut bytes = Vec::new();
        assert!(drive(&interaction, output, input, &mut bytes, async {
            interaction.notify(AuthEvent::Progress { message: "Synthetic progress".into() });
            Ok(credential())
        }).await);
        let frames: Vec<serde_json::Value> = bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).map(|line| serde_json::from_slice(line).unwrap()).collect();
        assert_eq!(frames, vec![serde_json::json!({"type":"event","event":{"type":"progress","message":"Synthetic progress"}}), serde_json::json!({"type":"complete","providerId":"synthetic-provider","credentialType":"API key"})]);
        let (interaction, output) = JsonInteraction::new();
        let (input, _parent) = tokio::io::duplex(32);
        let mut bytes = Vec::new();
        assert!(!drive(&interaction, output, input, &mut bytes, async {
            for _ in 0..9 { interaction.notify(AuthEvent::Progress { message: "Synthetic burst".into() }); }
            Ok(credential())
        }).await);
        assert_eq!(bytes, b"{\"type\":\"error\",\"code\":\"io\"}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn inherited_socket_transport_prompts_and_cancels_without_reopening() {
        let (input, mut parent_input) = std::os::unix::net::UnixStream::pair().unwrap();
        let (output, parent_output) = std::os::unix::net::UnixStream::pair().unwrap();
        let input = Transport::duplicate(&input).unwrap();
        let output = Transport::duplicate(&output).unwrap();
        let (interaction, receiver) = JsonInteraction::new();
        let operation = drive(&interaction, receiver, input, output, async {
            interaction.prompt(AuthPrompt::Text { message: "Synthetic prompt".into(), placeholder: None }).await?;
            Ok(credential())
        });
        parent_output.set_nonblocking(true).unwrap();
        let mut parent_output = tokio::net::UnixStream::from_std(parent_output).unwrap();
        let parent = async {
            let mut line = Vec::new();
            loop {
                let byte = parent_output.read_u8().await.unwrap();
                line.push(byte);
                if byte == b'\n' { break; }
            }
            let frame: serde_json::Value = serde_json::from_slice(&line).unwrap();
            assert_eq!(frame["type"], "prompt");
            parent_input.write_all(b"{\"type\":\"cancel\"}\n").unwrap();
        };
        let (success, ()) = tokio::time::timeout(std::time::Duration::from_secs(1), async { tokio::join!(operation, parent) }).await.unwrap();
        assert!(!success);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn undrained_socket_output_does_not_block_cancel_or_eof() {
        for eof in [false, true] {
            let (input, mut parent_input) = std::os::unix::net::UnixStream::pair().unwrap();
            let (output, parent_output) = std::os::unix::net::UnixStream::pair().unwrap();
            parent_output.set_nonblocking(true).unwrap();
            let mut parent_output = tokio::net::UnixStream::from_std(parent_output).unwrap();
            let input = Transport::duplicate(&input).unwrap();
            let output = Transport::duplicate(&output).unwrap();
            let (interaction, receiver) = JsonInteraction::new();
            let operation = drive(&interaction, receiver, input, output, async {
                interaction.notify(AuthEvent::Progress { message: "x".repeat(MAX_FRAME / 2) });
                std::future::pending::<Result<CredentialInfo>>().await
            });
            let parent = async {
                assert_eq!(parent_output.read_u8().await.unwrap(), b'{');
                if eof { drop(parent_input); } else { parent_input.write_all(b"{\"type\":\"cancel\"}\n").unwrap(); }
                // Keep stdout open but do not drain the large event.
                std::future::pending::<()>().await;
            };
            tokio::pin!(operation, parent);
            let success = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::select! { success = &mut operation => success, () = &mut parent => unreachable!() }
            }).await.unwrap();
            assert!(!success);
            assert_eq!(interaction.state.lock().failure, Some(Failure::Cancelled));
        }
    }
}
