import { useCallback, useEffect, useRef, useState } from 'react';
import {
  STT_AUTO_RELEASE_MS,
  STT_WAV_MIME,
  blobToWav,
  planSttCommit,
  planSttStart,
  planSttTranscribeResult,
  selectSttRecorderMime,
  shouldCommitSttTranscript,
  sttMicrophoneUnavailableMessage,
  sttTranscribeCommand,
  sttTranscribeFailedMessage,
  sttTranscriptText,
  wavToBase64,
} from './stt';

export type SttRecordingDestination =
  | { kind: 'MainComposer'; sessionId: string }
  | { kind: 'SideChatTab'; sessionId: string; tabId: string };

export type SttTranscriptRoute =
  | { kind: 'mainSubmit'; plan: 'steer' | 'prompt' }
  | { kind: 'sideChatInsert'; tabId: string }
  | { kind: 'drop' }
  | { kind: 'discardClosed' };

export function captureSttRecordingDestination(
  sessionId: string | null | undefined,
  target: 'MainComposer' | { kind: 'SideChatTab'; tabId: string | null | undefined },
): SttRecordingDestination | null {
  if (typeof sessionId !== 'string' || sessionId === '') return null;
  if (target === 'MainComposer') return { kind: 'MainComposer', sessionId };
  const tabId = target.tabId;
  if (typeof tabId !== 'string' || tabId === '') return null;
  return { kind: 'SideChatTab', sessionId, tabId };
}

export function planSttTranscriptRoute(args: {
  destination: SttRecordingDestination | null | undefined;
  owner: string;
  currentSessionId: string | null | undefined;
  sideChatPanelOpen: boolean;
  sideChatTabs: readonly string[];
  text: string;
  streaming: boolean;
}): SttTranscriptRoute {
  const dest = args.destination;
  if (!dest || dest.sessionId !== args.owner) return { kind: 'drop' };
  if (!shouldCommitSttTranscript(args.owner, args.currentSessionId)) return { kind: 'drop' };
  if (!args.text) return { kind: 'drop' };
  if (dest.kind === 'MainComposer') {
    const plan = planSttCommit(args.owner, args.currentSessionId, args.streaming);
    if (!plan) return { kind: 'drop' };
    return { kind: 'mainSubmit', plan };
  }
  if (!args.sideChatPanelOpen || !args.sideChatTabs.includes(dest.tabId)) {
    return { kind: 'discardClosed' };
  }
  return { kind: 'sideChatInsert', tabId: dest.tabId };
}

export type SttRecorderSendCommand = (command: Record<string, unknown>) => Promise<unknown>;

export type UseSttRecorderArgs = {
  getOwner: () => string | null | undefined;
  getDestination?: () => SttRecordingDestination | null | undefined;
  sttConfigured: { sttConfigured?: boolean } | null;
  sendCommand: SttRecorderSendCommand;
  onTranscript: (text: string, owner: string, destination: SttRecordingDestination | null) => void;
  onError: (message: string) => void;
};

type SttCapturePhase = 'idle' | 'acquiring' | 'recording' | 'stopping' | 'transcribing';

export function useSttRecorder({
  getOwner,
  getDestination,
  sttConfigured,
  sendCommand,
  onTranscript,
  onError,
}: UseSttRecorderArgs): {
  recording: boolean;
  transcribing: boolean;
  starting: boolean;
  isBusy: () => boolean;
  startRecording: () => Promise<void>;
  stopRecording: () => void;
} {
  const ownerRef = useRef(getOwner);
  const destinationRef = useRef(getDestination);
  const configuredRef = useRef(sttConfigured);
  const sendCommandRef = useRef(sendCommand);
  const onTranscriptRef = useRef(onTranscript);
  const onErrorRef = useRef(onError);
  ownerRef.current = getOwner;
  destinationRef.current = getDestination;
  configuredRef.current = sttConfigured;
  sendCommandRef.current = sendCommand;
  onTranscriptRef.current = onTranscript;
  onErrorRef.current = onError;

  const streamRef = useRef<MediaStream | null>(null);
  const recorderRef = useRef<MediaRecorder | null>(null);
  const chunksRef = useRef<Blob[]>([]);
  const timerRef = useRef<number | null>(null);
  const captureOwnerRef = useRef<string | null>(null);
  const attemptRef = useRef(0);
  const phaseRef = useRef<SttCapturePhase>('idle');
  const unmountedRef = useRef(false);
  const [phase, setPhaseState] = useState<SttCapturePhase>('idle');
  const setPhase = useCallback((next: SttCapturePhase) => {
    phaseRef.current = next;
    setPhaseState(next);
  }, []);

  const stopRecording = useCallback(() => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    if (phaseRef.current === 'acquiring') {
      attemptRef.current += 1;
      captureOwnerRef.current = null;
      setPhase('idle');
      return;
    }
    if (phaseRef.current !== 'recording') return;
    const attempt = attemptRef.current;
    const recorder = recorderRef.current;
    recorderRef.current = null;
    if (streamRef.current) {
      streamRef.current.getTracks().forEach((track) => track.stop());
      streamRef.current = null;
    }
    setPhase('stopping');
    if (recorder && recorder.state !== 'inactive') {
      try {
        recorder.stop();
      } catch (err) {
        if (attemptRef.current !== attempt) return;
        setPhase('idle');
        const captured = captureOwnerRef.current;
        if (shouldCommitSttTranscript(captured, ownerRef.current())) {
          onErrorRef.current(sttTranscribeFailedMessage(err));
        }
      }
      return;
    }
    if (attemptRef.current === attempt) setPhase('idle');
  }, [setPhase]);

  const transcribeAudio = useCallback(async (
    audioBlob: Blob,
    captured: string,
    destination: SttRecordingDestination | null,
    attempt: number,
  ) => {
    if (attemptRef.current !== attempt) return;
    setPhase('transcribing');
    try {
      const wav = await blobToWav(audioBlob);
      if (attemptRef.current !== attempt) return;
      if (!wav) {
        const result = planSttTranscribeResult({
          owner: captured,
          current: ownerRef.current(),
          wavOk: false,
          audioBase64: null,
          text: '',
        });
        if (result.kind === 'error') onErrorRef.current(result.message);
        return;
      }
      const audioBase64 = wavToBase64(new Uint8Array(await wav.arrayBuffer()));
      if (attemptRef.current !== attempt) return;
      if (audioBase64 === null) {
        const result = planSttTranscribeResult({
          owner: captured,
          current: ownerRef.current(),
          wavOk: true,
          audioBase64: null,
          text: '',
        });
        if (result.kind === 'error') onErrorRef.current(result.message);
        return;
      }
      const raw = await sendCommandRef.current(sttTranscribeCommand(captured, audioBase64, STT_WAV_MIME));
      if (attemptRef.current !== attempt) return;
      const result = planSttTranscribeResult({
        owner: captured,
        current: ownerRef.current(),
        wavOk: true,
        audioBase64,
        text: sttTranscriptText(raw),
      });
      if (result.kind === 'error') onErrorRef.current(result.message);
      else if (result.kind === 'transcript') onTranscriptRef.current(result.text, captured, destination);
    } catch (err) {
      if (attemptRef.current !== attempt) return;
      if (shouldCommitSttTranscript(captured, ownerRef.current())) {
        onErrorRef.current(sttTranscribeFailedMessage(err));
      }
    } finally {
      if (attemptRef.current !== attempt) return;
      if (captureOwnerRef.current === captured) captureOwnerRef.current = null;
      setPhase('idle');
    }
  }, [setPhase]);

  const startRecording = useCallback(async () => {
    if (phaseRef.current !== 'idle') return;
    const planned = planSttStart({
      owner: ownerRef.current(),
      live: configuredRef.current,
      hasGetUserMedia: !!navigator.mediaDevices?.getUserMedia,
    });
    if (!planned.ok) {
      onErrorRef.current(planned.message);
      return;
    }
    const captured = planned.owner;
    const capturedDestination = destinationRef.current?.() ?? null;
    captureOwnerRef.current = captured;
    const attempt = ++attemptRef.current;
    setPhase('acquiring');
    let stream: MediaStream | null = null;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      if (
        attemptRef.current !== attempt
        || unmountedRef.current
        || !shouldCommitSttTranscript(captured, ownerRef.current())
      ) {
        stream.getTracks().forEach((track) => track.stop());
        if (attemptRef.current === attempt) {
          if (captureOwnerRef.current === captured) captureOwnerRef.current = null;
          setPhase('idle');
        }
        return;
      }
      const mime = selectSttRecorderMime(
        typeof MediaRecorder !== 'undefined' ? (candidate) => MediaRecorder.isTypeSupported(candidate) : undefined,
      );
      const recorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
      streamRef.current = stream;
      recorderRef.current = recorder;
      chunksRef.current = [];
      recorder.ondataavailable = (e) => {
        if (e.data && e.data.size > 0) chunksRef.current.push(e.data);
      };
      recorder.onstop = () => {
        if (attemptRef.current !== attempt) return;
        const chunks = chunksRef.current;
        chunksRef.current = [];
        if (unmountedRef.current || chunks.length === 0) {
          if (captureOwnerRef.current === captured) captureOwnerRef.current = null;
          setPhase('idle');
          return;
        }
        const blob = new Blob(chunks, { type: recorder.mimeType || 'audio/webm' });
        void transcribeAudio(blob, captured, capturedDestination, attempt);
      };
      setPhase('recording');
      recorder.start();
      timerRef.current = window.setTimeout(() => stopRecording(), STT_AUTO_RELEASE_MS);
    } catch (err) {
      if (attemptRef.current !== attempt) {
        if (stream) stream.getTracks().forEach((track) => track.stop());
        return;
      }
      setPhase('idle');
      if (captureOwnerRef.current === captured) captureOwnerRef.current = null;
      if (stream) stream.getTracks().forEach((track) => track.stop());
      if (streamRef.current) {
        streamRef.current.getTracks().forEach((track) => track.stop());
        streamRef.current = null;
      }
      if (shouldCommitSttTranscript(captured, ownerRef.current())) {
        onErrorRef.current(sttMicrophoneUnavailableMessage(err));
      }
    }
  }, [setPhase, stopRecording, transcribeAudio]);

  useEffect(() => {
    unmountedRef.current = false;
    return () => {
      unmountedRef.current = true;
      attemptRef.current += 1;
      phaseRef.current = 'idle';
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      const recorder = recorderRef.current;
      recorderRef.current = null;
      if (streamRef.current) {
        streamRef.current.getTracks().forEach((track) => track.stop());
        streamRef.current = null;
      }
      if (recorder && recorder.state !== 'inactive') {
        try {
          recorder.stop();
        } catch (err) {
          onErrorRef.current(sttTranscribeFailedMessage(err));
        }
      }
    };
  }, []);

  const recording = phase === 'recording' || phase === 'stopping';
  const transcribing = phase === 'transcribing';
  const starting = phase === 'acquiring';
  return {
    recording,
    transcribing,
    starting,
    isBusy: () => phaseRef.current !== 'idle',
    startRecording,
    stopRecording,
  };
}
