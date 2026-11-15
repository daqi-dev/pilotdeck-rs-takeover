import { createStaffDeckRequestContext } from './adapters/staffdeck-request-context.js';

export function bindModuleRequestAbort(req, res) {
  const context = createStaffDeckRequestContext(req, res);
  const cleanup = () => {
    context.dispose();
    res.off('close', cleanup);
    res.off('finish', cleanup);
  };
  res.once('close', cleanup);
  res.once('finish', cleanup);
  return context.signal;
}

export function moduleUpstreamSignal(signal, timeoutMs) {
  const timeout = Number.isFinite(timeoutMs) && timeoutMs > 0 ? AbortSignal.timeout(timeoutMs) : undefined;
  if (signal && timeout) return AbortSignal.any([signal, timeout]);
  return signal ?? timeout;
}
