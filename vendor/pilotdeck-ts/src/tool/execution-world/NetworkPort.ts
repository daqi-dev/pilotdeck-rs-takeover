/** Session-owned network policy, supplied by host execution composition. */
export type NetworkPort = {
  fetch: typeof fetch;
};

export function createDeniedNetworkPort(): NetworkPort {
  return {
    fetch: async () => { throw new Error("Session network egress denied"); },
  };
}
