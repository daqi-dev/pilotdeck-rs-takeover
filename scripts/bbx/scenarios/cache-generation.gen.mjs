// 生成 ctx-cache-generation-{session-ops,reload,idle}.json：让提示缓存代际回到 1 的其余入口。
// 用法（仓库根目录）：node scripts/bbx/scenarios/cache-generation.gen.mjs
// 配置与假 provider 取自手写的 ctx-cache-generation.json，MCP 回显服务取自 mcp-session-servers.json。
import fs from 'node:fs';
import path from 'node:path';

const dir = path.dirname(new URL(import.meta.url).pathname);
const base = JSON.parse(fs.readFileSync(path.join(dir, 'ctx-cache-generation.json'), 'utf8'));
const mcp = JSON.parse(fs.readFileSync(path.join(dir, 'mcp-session-servers.json'), 'utf8'));
const echo = mcp.files['home/.pilotdeck/mcp/echo-server.mjs'];

let n = 0;
const turn = (session, extra = {}) => [
  { ws: 'submit_turn', params: { sessionKey: session, channelKey: 'test', message: `${session} t${++n}`, ...extra } },
  { sleep: 300 },
];
const ws = (method, params) => ({ ws: method, params });
const read = (session) => ws('read_session_messages', { sessionKey: session });
const ECHO = { 'bbx-sdk': { type: 'stdio', command: 'node', args: ['{PILOT_HOME}/mcp/echo-server.mjs', 'sdk'] } };

const out = {};

n = 0;
out['ctx-cache-generation-session-ops'] = {
  about: '会话级操作与提示缓存代际（PromptCacheCoordinator 挂在会话的上下文运行时上，会话被关闭或重建后从 1 重新数，见 ctx-cache-generation）。'
    + '基线里关闭会话的入口：set_mcp_servers / mcp_server_reconnect / mcp_server_toggle / set_mcp_permission_mode_override 成功后（InProcessGateway.ts:2284-2318），出错不关；'
    + 'apply_flag_settings 只在 applied 或 cleared 非空时（InProcessGateway.ts:2337-2347）；set_output_style（ProjectRuntimeRegistry.ts:1107）；'
    + 'reload_output_styles 关有 sdkSessionConfig 的会话（ProjectRuntimeRegistry.ts:1112-1122）；seed_read_state 带变化的 sdkSessionConfig（InProcessGateway.ts:2430-2436）；'
    + 'submit_turn 的 allowedTools / disallowedTools 变化让会话重建（SessionRouter.ts:137-167、561-572），不带过滤器的一轮不重建。模型一律回 400。',
  config: base.config,
  files: { 'home/.pilotdeck/mcp/echo-server.mjs': echo, 'home/.pilotdeck/seed.txt': 'seeded\n' },
  provider: base.provider,
  steps: [
    // apply_flag_settings
    ...turn('s-flag'), ...turn('s-flag'),
    ws('apply_flag_settings', { sessionKey: 's-flag', settings: {} }),
    ...turn('s-flag'),
    ws('apply_flag_settings', { sessionKey: 's-flag', settings: { effortLevel: 'low' } }),
    ...turn('s-flag'), ...turn('s-flag'),
    ws('apply_flag_settings', { sessionKey: 's-flag', settings: { effortLevel: null } }),
    ...turn('s-flag'),
    // output style
    ...turn('s-style'), ...turn('s-style'),
    ws('set_output_style', { sessionKey: 's-style', name: null }),
    ...turn('s-style'), ...turn('s-style'),
    ws('reload_output_styles', {}),
    ...turn('s-style'),
    // tool filters
    ...turn('s-filter', { allowedTools: ['read_file', 'glob'] }),
    ...turn('s-filter', { allowedTools: ['read_file', 'glob'] }),
    ...turn('s-filter'),
    ...turn('s-filter', { allowedTools: ['glob'] }),
    ...turn('s-filter', { disallowedTools: ['glob'] }),
    ...turn('s-filter'),
    // seed_read_state with an sdkSessionConfig
    ...turn('s-seed'), ...turn('s-seed'),
    // A changed config closes the session before the (failing) seed itself.
    ws('seed_read_state', { sessionKey: 's-seed', path: 'missing.txt', mtime: 0, sdkSessionConfig: { appendSystemPrompt: 'SEED-APPEND' } }),
    ...turn('s-seed'), ...turn('s-seed'),
    ws('seed_read_state', { sessionKey: 's-seed', path: 'seed.txt', mtime: 0, sdkSessionConfig: { appendSystemPrompt: 'SEED-APPEND' } }),
    ...turn('s-seed'),
    ws('seed_read_state', { sessionKey: 's-seed', path: 'seed.txt', mtime: 0, sdkSessionConfig: { appendSystemPrompt: 'SEED-OTHER' } }),
    ...turn('s-seed'),
    // MCP: errors keep the session, successes close it
    ...turn('s-mcp'), ...turn('s-mcp'),
    ws('mcp_server_toggle', { sessionKey: 's-mcp', serverName: 'ghost', enabled: false }),
    ws('mcp_server_reconnect', { sessionKey: 's-mcp', serverName: 'ghost' }),
    ...turn('s-mcp'),
    ws('set_mcp_servers', { sessionKey: 's-mcp', servers: ECHO }),
    ...turn('s-mcp'), ...turn('s-mcp'),
    ws('mcp_server_reconnect', { sessionKey: 's-mcp', serverName: 'bbx-sdk' }),
    ...turn('s-mcp'), ...turn('s-mcp'),
    ws('mcp_server_toggle', { sessionKey: 's-mcp', serverName: 'bbx-sdk', enabled: false }),
    ...turn('s-mcp'), ...turn('s-mcp'),
    ws('set_mcp_permission_mode_override', { sessionKey: 's-mcp', serverName: 'bbx-sdk', mode: 'auto' }),
    ...turn('s-mcp'), ...turn('s-mcp'),
    ws('set_mcp_servers', { sessionKey: 's-mcp', servers: {} }),
    ...turn('s-mcp'),
  ],
  volatile: base.volatile,
};

n = 0;
out['ctx-cache-generation-reload'] = {
  about: '项目级刷新与提示缓存代际：reload_extensions 把全部活会话标脏（GatewayRuntimeRefreshBundle.ts:84-106），下一轮重建、代际回到 1；'
    + 'update_settings 改了非 runPolicy 的配置后 ConfigStore 重载、运行时刷新完再把全部会话标脏（GatewayRuntimeRefreshBundle.ts:137-169）；'
    + '没有变化的 reload_config 不标脏。两个会话一起看。模型一律回 400。',
  config: base.config,
  provider: base.provider,
  steps: [
    ...turn('s-a'), ...turn('s-a'), ...turn('s-b'),
    ws('reload_config', {}),
    ...turn('s-a'), ...turn('s-b'),
    ws('reload_extensions', {}),
    ...turn('s-a'), ...turn('s-a'), ...turn('s-b'),
    ws('update_settings', { source: 'localSettings', settings: { agent: { maxOutputTokens: 4096 } } }),
    { sleep: 1000 },
    ...turn('s-a'), ...turn('s-b'), ...turn('s-b'),
  ],
  volatile: base.volatile,
};

n = 0;
const idleConfig = base.config.replace('telemetry:\n  enabled: false\n', 'telemetry:\n  enabled: false\ngateway:\n  idleSessionTimeoutMinutes: 0.02\n  idleSweepIntervalSeconds: 0.2\n');
if (idleConfig === base.config) throw new Error('idle config not inserted');
out['ctx-cache-generation-idle'] = {
  about: '空闲回收与提示缓存代际：gateway.idleSessionTimeoutMinutes=0.02（1.2 秒）、idleSweepIntervalSeconds=0.2（createLocalGateway.ts:827-831 → SessionRouter.ts:471-485）。'
    + '连着的几轮间隔 0.3 秒，代际递增；空闲 2.5 秒后会话被逐出，下一轮重新组装，代际回到 1。模型一律回 400。',
  config: idleConfig,
  provider: base.provider,
  steps: [
    ...turn('s-idle'), ...turn('s-idle'), ...turn('s-idle'),
    { sleep: 2500 },
    ...turn('s-idle'), ...turn('s-idle'),
    { sleep: 2500 },
    ...turn('s-idle'),
  ],
  volatile: base.volatile,
};

for (const [name, body] of Object.entries(out)) {
  const scenario = { name, ...body };
  if (!scenario.volatile) delete scenario.volatile;
  fs.writeFileSync(path.join(dir, `${name}.json`), JSON.stringify(scenario, null, 2) + '\n');
  console.log('wrote', name);
}
