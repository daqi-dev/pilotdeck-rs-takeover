import { useState } from 'react';
import { act, cleanup, renderHook } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { useChatRealtimeHandlers } from '@pilotdeck-baseline/ui/src/components/chat/hooks/useChatRealtimeHandlers';
import { buildSessionStatusRequest, resetSessionStatusProtocolForTests } from '@pilotdeck-baseline/ui/src/components/chat/sessionStatusProtocol';

const wire = vi.hoisted(() => ({ listener: null as any, sendMessage: vi.fn() }));
vi.mock('@pilotdeck-baseline/ui/src/contexts/WebSocketContext', () => ({
  useWebSocket: () => ({ sendMessage: wire.sendMessage, subscribe: (listener: any) => { wire.listener = listener; return () => {}; } }),
}));
vi.mock('@pilotdeck-baseline/ui/src/stores/useSessionStore', () => ({
  normalizeCompactionMessage: (message: any) => message,
  getUnpersistedRealtimeTurnMessages: () => [],
  isRealtimeMessageRepresentedOnServer: () => false,
}));
const sid = 'web:recovery';
const noop = () => {};
const request = (sessionId: string, runId: string | undefined, requestId: string) => ({ sessionId, runId, requestId, toolName: 'write_file' });
function mount(initial: any[] = [], run: string | null = 'old-run') {
  const store: any = {
    appendRealtime: vi.fn(), closeTimeline: vi.fn(), setActiveSession: vi.fn(),
    cancelRunningActivities: vi.fn(), finalizeStreaming: vi.fn(), finalizeStreamingThinking: vi.fn(),
    refreshFromServer: vi.fn().mockResolvedValue(undefined),
    getSessionSlot: () => ({ realtimeMessages: [], serverMessages: [] }),
  };
  const state = renderHook(() => {
    const [pending, setPending] = useState(initial);
    const [activeRunId, setActiveRunId] = useState(run);
    const [runtime, setRuntime] = useState<any>('running');
    useChatRealtimeHandlers({
      provider: 'pilotdeck', selectedProject: null, selectedSession: { id: sid } as any,
      currentSessionId: sid, setCurrentSessionId: noop, setIsLoading: noop,
      setSessionRuntimeState: setRuntime, activeRunId, setActiveRunId,
      setCanAbortSession: noop, setIsAborting: noop, setClaudeStatus: noop,
      setPilotDeckStatus: noop, setTokenBudget: noop, setPendingPermissionRequests: setPending,
      pendingViewSessionRef: { current: null }, sessionStore: store,
    });
    return { pending, activeRunId, setActiveRunId, runtime };
  });
  return { ...state, store };
}
function deliver(message: any) { act(() => wire.listener(message)); }
function reply(request: any, isProcessing: boolean | null, extra = {}) {
  deliver({ ...request, type: 'session-status', isProcessing, ...extra });
}
beforeEach(() => { wire.sendMessage.mockReset(); resetSessionStatusProtocolForTests(); });
afterEach(() => { cleanup(); vi.useRealTimers(); });

it.each([false, true])('reconciles the exact permission/question run after transport loss (elicitation=%s)', isElicitation => {
  const { result, store } = mount([request('web:other', 'old-run', 'same-id')]);
  deliver({ kind: 'permission_request', sessionId: sid, runId: 'old-run', requestId: 'same-id', isElicitation });
  expect(result.current.pending.find(item => item.sessionId === sid)).toMatchObject({ runId: 'old-run', isElicitation });
  deliver({ kind: 'error', terminal: true, code: 'gateway_unavailable', sessionId: sid });
  expect(result.current.pending).toHaveLength(2);
  expect(result.current.activeRunId).toBe('old-run');
  expect(store.closeTimeline).not.toHaveBeenCalled();
  const statusRequest = wire.sendMessage.mock.calls.at(-1)![0];
  expect(statusRequest).toMatchObject({ type: 'check-session-status', expectedActiveRunId: 'old-run' });
  reply(statusRequest, false);
  expect(result.current.pending).toEqual([request('web:other', 'old-run', 'same-id')]);
  expect(result.current.activeRunId).toBe(null);
});

it('preserves a live pending interaction across unknown and active recovery snapshots', () => {
  vi.useFakeTimers();
  const { result } = mount([request(sid, 'old-run', 'permission')]);
  deliver({ kind: 'error', terminal: true, code: 'gateway_unavailable', sessionId: sid });
  const first = wire.sendMessage.mock.calls.at(-1)![0];
  reply(first, null);
  expect(result.current.pending).toHaveLength(1);
  expect(result.current.runtime).toBe('synchronizing');
  act(() => vi.advanceTimersByTime(1300));
  const retried = wire.sendMessage.mock.calls.at(-1)![0];
  expect(retried.statusRequestId).toBeGreaterThan(first.statusRequestId);
  reply(retried, true, { activeRunId: 'old-run' });
  expect(result.current.pending).toHaveLength(1);
  expect(result.current.activeRunId).toBe('old-run');
  expect(result.current.runtime).toBe('running');
});

it('rejects an old-run inactive reply after a new run started', () => {
  const { result } = mount([request(sid, 'old-run', 'old'), request(sid, 'new-run', 'new')]);
  const status = buildSessionStatusRequest({ sessionId: sid, provider: 'pilotdeck', expectedActiveRunId: 'old-run', includeActiveTurnMessages: true });
  act(() => result.current.setActiveRunId('new-run'));
  reply(status, false);
  expect(result.current.pending.map(item => item.requestId)).toEqual(['old', 'new']);
  expect(result.current.activeRunId).toBe('new-run');
});

it('does not clear pending from an unsolicited legacy inactive frame', () => {
  const { result } = mount([request(sid, 'old-run', 'old')]);
  deliver({ type: 'session-status', sessionId: sid, expectedActiveRunId: 'old-run', isProcessing: false });
  expect(result.current.pending).toHaveLength(1);
});

it.each(['complete', 'error'])('settles only the identified old run on %s, preserving another session and newer run', kind => {
  const { result } = mount([request(sid, 'old-run', 'old'), request(sid, 'new-run', 'new'), request('web:other', 'old-run', 'other')], 'new-run');
  deliver({ kind, terminal: true, sessionId: sid, runId: 'old-run' });
  expect(result.current.pending.map(item => item.requestId)).toEqual(['new', 'other']);
  expect(result.current.activeRunId).toBe('new-run');
});

it('matches explicit cancellation by session/request and protects a different known run', () => {
  const { result } = mount([request(sid, 'old-run', 'same'), request(sid, 'new-run', 'same'), request('web:other', 'old-run', 'same')]);
  deliver({ kind: 'permission_cancelled', sessionId: sid, runId: 'old-run', requestId: 'same' });
  expect(result.current.pending).toEqual([request(sid, 'new-run', 'same'), request('web:other', 'old-run', 'same')]);
});

it('settles only unbound legacy permissions on unscoped completion when no known run is active', () => {
  const { result } = mount([request(sid, undefined, 'legacy'), request(sid, 'new-run', 'new'), request('web:other', undefined, 'other')], null);
  deliver({ kind: 'complete', sessionId: sid });
  expect(result.current.pending.map(item => item.requestId)).toEqual(['new', 'other']);
});

it('preserves even unbound legacy permissions when a known newer run is active', () => {
  const { result } = mount([request(sid, undefined, 'legacy'), request(sid, 'new-run', 'new')], 'new-run');
  deliver({ kind: 'complete', sessionId: sid });
  expect(result.current.pending.map(item => item.requestId)).toEqual(['legacy', 'new']);
});

it('ignores a wrong status sequence before accepting the correlated inactive reply', () => {
  const { result } = mount([request(sid, 'old-run', 'old')]);
  const status = buildSessionStatusRequest({ sessionId: sid, provider: 'pilotdeck', expectedActiveRunId: 'old-run', includeActiveTurnMessages: true });
  reply({ ...status, statusRequestId: status.statusRequestId + 1 }, false);
  expect(result.current.pending).toHaveLength(1);
  reply(status, false);
  expect(result.current.pending).toEqual([]);
});

import { useChatComposerState } from '@pilotdeck-baseline/ui/src/components/chat/hooks/useChatComposerState';
vi.mock('@pilotdeck-baseline/ui/src/utils/api', () => ({
  authenticatedFetch: vi.fn().mockResolvedValue({ ok: true, json: async () => ({ pinned: [], builtIn: [], custom: [] }) }),
}));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));

it('merges a runless replay into the same known session/request without losing its run', () => {
  const { result } = mount([request(sid, 'old-run', 'permission')]);
  deliver({ kind: 'permission_request', sessionId: sid, requestId: 'permission', toolName: 'write_file' });
  expect(result.current.pending).toEqual([request(sid, 'old-run', 'permission')]);
});
it('enriches an unknown replay run when the identified request arrives', () => {
  const { result } = mount([request(sid, undefined, 'permission')]);
  deliver({ kind: 'permission_request', sessionId: sid, requestId: 'permission', runId: 'old-run', toolName: 'write_file' });
  expect(result.current.pending).toEqual([request(sid, 'old-run', 'permission')]);
});
it('does not merge conflicting known runs or another session using the same request ID', () => {
  const initial = [request(sid, 'old-run', 'permission'), request('web:other', 'new-run', 'permission')];
  const { result } = mount(initial, 'new-run');
  deliver({ kind: 'permission_request', sessionId: sid, requestId: 'permission', runId: 'new-run', toolName: 'write_file' });
  expect(result.current.pending).toHaveLength(3);
  expect(result.current.pending.slice(0, 2)).toEqual(initial);
  expect(result.current.pending[2]).toMatchObject({ sessionId: sid, runId: 'new-run' });
});
function mountComposer(isElicitation: boolean) {
  const statuses = { claude: vi.fn(), pilot: vi.fn() };
  const store: any = {
    appendRealtime: noop, closeTimeline: noop, setActiveSession: noop,
    cancelRunningActivities: noop, finalizeStreaming: noop, finalizeStreamingThinking: noop,
    refreshFromServer: vi.fn().mockResolvedValue(undefined),
    getSessionSlot: () => ({ realtimeMessages: [], serverMessages: [] }),
  };
  const view = renderHook(() => {
    const [pending, setPending] = useState([{ ...request(sid, 'old-run', 'decision'), isElicitation }]);
    const [activeRunId, setActiveRunId] = useState<string | null>('old-run');
    const composer = useChatComposerState({
      selectedProject: null, selectedSession: { id: sid } as any, currentSessionId: sid,
      model: 'mock/main', modelSelection: { mode: 'model', provider: 'mock', model: 'main' },
      isModelSelectionReady: true, isPermissionModeReady: true,
      permissionMode: 'default', runMode: 'agent', cycleRunMode: noop, isLoading: true,
      canAbortSession: true, tokenBudget: null, sendMessage: wire.sendMessage,
      pendingViewSessionRef: { current: null }, scrollToBottom: noop, addMessage: noop,
      clearMessages: noop, rewindMessages: noop, setIsLoading: noop, setCanAbortSession: noop,
      setIsAborting: noop, setClaudeStatus: statuses.claude, setPilotDeckStatus: statuses.pilot, setIsUserScrolledUp: noop,
      pendingPermissionRequests: pending, setPendingPermissionRequests: setPending,
    });
    useChatRealtimeHandlers({
      provider: 'pilotdeck', selectedProject: null, selectedSession: { id: sid } as any,
      currentSessionId: sid, setCurrentSessionId: noop, setIsLoading: noop,
      setSessionRuntimeState: noop, activeRunId, setActiveRunId,
      setCanAbortSession: noop, setIsAborting: noop, setClaudeStatus: statuses.claude,
      setPilotDeckStatus: statuses.pilot, setTokenBudget: noop, setPendingPermissionRequests: setPending,
      pendingViewSessionRef: { current: null }, sessionStore: store,
    });
    return { pending, decide: composer.handlePermissionDecision };
  });
  statuses.claude.mockClear(); statuses.pilot.mockClear();
  return { ...view, statuses };
}
it.each([
  [false, false], [false, true], [true, false], [true, true],
])('waits for ACK after the real composer response (question=%s, send=%s)', (isElicitation, sent) => {
  wire.sendMessage.mockReturnValue(sent);
  const { result, statuses } = mountComposer(isElicitation);
  act(() => result.current.decide('decision', { allow: true, updatedInput: { answers: { question: 'A' } } }));
  expect(wire.sendMessage).toHaveBeenCalledWith(expect.objectContaining({
    type: isElicitation ? 'elicitation-response' : 'permission-response', requestId: 'decision', sessionId: sid,
  }));
  expect(result.current.pending).toHaveLength(1);
  expect(statuses.claude).not.toHaveBeenCalled();
  expect(statuses.pilot).not.toHaveBeenCalled();
  if (sent) {
    deliver({ kind: 'permission_cancelled', sessionId: sid, requestId: 'decision' });
    expect(result.current.pending).toEqual([]);
    expect(statuses.claude).toHaveBeenLastCalledWith(null);
    expect(statuses.pilot).toHaveBeenLastCalledWith(null);
  }
});
it('ignores another-session and duplicate ACK without clearing the current Waiting state', () => {
  const { result, statuses } = mountComposer(true);
  deliver({ kind: 'permission_cancelled', sessionId: 'web:other', requestId: 'decision' });
  expect(result.current.pending).toHaveLength(1);
  expect(statuses.claude).not.toHaveBeenCalled();
  deliver({ kind: 'permission_cancelled', sessionId: sid, requestId: 'decision' });
  statuses.claude.mockClear(); statuses.pilot.mockClear();
  deliver({ kind: 'permission_cancelled', sessionId: sid, requestId: 'decision' });
  expect(statuses.claude).not.toHaveBeenCalled();
  expect(statuses.pilot).not.toHaveBeenCalled();
});
