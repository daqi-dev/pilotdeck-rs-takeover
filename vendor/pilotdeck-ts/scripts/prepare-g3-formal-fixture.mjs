import { prepareStaffDeckBindings } from './prepare-staffdeck-bindings.mjs';

/** Fresh per-state formal identities, obtained exclusively through normal APIs. */
export async function prepareG3FormalFixture({ origin, pdOrigin, account, gatewayUrl, gatewayTokenPath, definitionsPath, stateId }) {
  async function request(base, path, token, body) {
    const response = await fetch(new URL(path, base), {
      method: body === undefined ? 'GET' : 'POST', redirect: 'error',
      headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...(body === undefined ? {} : { 'content-type': 'application/json' }) },
      body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(30_000),
    });
    if (!response.ok) throw new Error(`G3 formal fixture ${path} failed with HTTP ${response.status}`);
    return response.json();
  }
  const actorLogin = await request(origin, '/api/auth/login', undefined, { tenant_id: 'tenant_demo', username: 'admin', password: 'admin' });
  const actorMe = await request(origin, '/api/auth/me', actorLogin.token);
  const target = await request(origin, '/api/enterprise/agents', actorLogin.token, { tenant_id: actorMe.tenant_id, name: `G3 ${stateId}`, source_mode: 'blank', is_overall: false });
  const credentialCreated = await request(origin, '/api/auth/me/api-credentials', actorLogin.token, { name: `G3 ${stateId}` });
  const credentials = await request(origin, '/api/auth/me/api-credentials', actorLogin.token);
  const pilotDeckMe = await request(pdOrigin, '/api/auth/user', account.token);
  return prepareStaffDeckBindings({ actorLogin, actorMe, target, credentialCreated, credentials,
    pilotDeckLogin: account, pilotDeckMe, staffDeckOrigin: origin, pilotDeckGatewayUrl: gatewayUrl,
    pilotDeckGatewayTokenPath: gatewayTokenPath, definitionsPath, defaultSopId: 'g3_probe' });
}
