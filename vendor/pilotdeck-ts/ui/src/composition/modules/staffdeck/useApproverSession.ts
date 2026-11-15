import { useCallback, useEffect, useRef, useState } from 'react';
import { authenticatedFetch } from '../../../utils/api';
import { moduleApiError } from './clients';

type ApproverUser = { id: string; username: string; tenant_id: string };

/** A normal StaffDeck session, scoped to the current PilotDeck login and kept only in memory. */
export function useApproverSession(ownerId: string | null) {
  const session = useRef<{ ownerId: string; token: string; user: ApproverUser } | null>(null);
  const pending = useRef<AbortController | null>(null);
  const generation = useRef(0);
  const [user, setUser] = useState<ApproverUser | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  const logout = useCallback(() => {
    generation.current++;
    pending.current?.abort();
    session.current = null;
    setUser(null);
    setLoading(false);
    setError('');
    setRevision(value => value + 1);
  }, []);
  useEffect(() => {
    logout();
    return () => { generation.current++; pending.current?.abort(); session.current = null; };
  }, [ownerId, logout]);
  const readApproverBearer = useCallback(() => session.current?.ownerId === ownerId ? session.current.token : '', [ownerId]);
  const request = useCallback(async (username?: string, password?: string) => {
    if (!ownerId) return;
    const token = readApproverBearer();
    const login = username !== undefined;
    if (!login && !token) return;
    const current = ++generation.current;
    pending.current?.abort();
    const controller = new AbortController();
    pending.current = controller;
    setLoading(true);
    setError('');
    try {
      const response = await authenticatedFetch(`/api/sop/approver/${login ? 'login' : 'session'}`, {
        signal: controller.signal,
        ...(login ? { method: 'POST', body: JSON.stringify({ username, password }) }
          : { headers: { 'X-StaffDeck-Approver-Authorization': `Bearer ${token}` } }),
      });
      const raw = await response.text();
      if (!response.ok) throw moduleApiError(response.status, raw, response.statusText);
      const result = JSON.parse(raw);
      if (!result.user?.id || typeof result.user.username !== 'string'
        || !result.user.tenant_id || (login && (typeof result.token !== 'string' || !result.token))) {
        throw new Error('APPROVAL_SESSION_INVALID');
      }
      if (current !== generation.current || controller.signal.aborted) return;
      session.current = { ownerId, token: login ? result.token : token, user: result.user };
      setUser(result.user);
      setRevision(value => value + 1);
    } catch (cause) {
      if (current !== generation.current || controller.signal.aborted) return;
      session.current = null;
      setUser(null);
      setRevision(value => value + 1);
      setError(cause instanceof Error ? (typeof (cause as { code?: unknown }).code === 'string'
        ? String((cause as { code?: string }).code) : cause.message) : String(cause));
    } finally {
      if (current === generation.current && !controller.signal.aborted) setLoading(false);
    }
  }, [ownerId, readApproverBearer]);
  useEffect(() => {
    const revalidate = () => { void request(); };
    window.addEventListener('focus', revalidate);
    return () => window.removeEventListener('focus', revalidate);
  }, [request]);
  return { user: session.current?.ownerId === ownerId ? user : null, loading, error, revision,
    readApproverBearer, login: request, logout };
}
