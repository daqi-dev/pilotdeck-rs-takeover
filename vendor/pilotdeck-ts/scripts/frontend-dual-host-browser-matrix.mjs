import { createRequire } from 'node:module';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

// Resolve the browser runtime from PilotDeck/ui's declared lockfile package.
const requireFromUi = createRequire(new URL('../ui/package.json', import.meta.url));
const { chromium } = requireFromUi('@playwright/test');
const [, , inputPath, outputPath] = process.argv;
if (!inputPath || !outputPath) throw new Error('Usage: node scripts/frontend-dual-host-browser-matrix.mjs <local-identity.json> <output-dir>');
const input = JSON.parse(await readFile(inputPath, 'utf8'));
const { pilotDeckOrigin, staffDeckOrigin, pilotDeckLogin, actorLogin, target, knowledgeTitle, sopId, draftId } = input;
if (![pilotDeckOrigin, staffDeckOrigin, pilotDeckLogin?.token, actorLogin?.token, target?.id, knowledgeTitle, sopId, draftId].every(Boolean)) {
  throw new Error('Local matrix input requires two origins, authenticated identities, target, knowledgeTitle, sopId and the actual draftId.');
}
const root = resolve(outputPath);
await mkdir(root, { recursive: true });
const routes = { pd: ['/knowledge', '/knowledge/new', '/sop', `/sop/distill?skill_id=${encodeURIComponent(sopId)}`],
  sd: ['/enterprise/knowledge', '/enterprise/knowledge/new', '/enterprise/skills', `/enterprise/skills/distill?skill_id=${encodeURIComponent(sopId)}&draft_id=${encodeURIComponent(draftId)}&agent_id=${encodeURIComponent(target.id)}`] };
const onlyCases = new Set((process.env.MATRIX_CASES || '').split(',').filter(Boolean));
const rows = [];
const browser = await chromium.launch({ headless: true });
try {
  for (const host of ['pd', 'sd']) for (const language of ['zh', 'en']) for (const theme of ['light', 'dark']) for (const viewport of ['desktop', 'mobile']) {
    const id = [host, language, theme, viewport].join('-');
    if (onlyCases.size && !onlyCases.has(id)) continue;
    const context = await browser.newContext({ viewport: viewport === 'desktop' ? { width: 1440, height: 1000 } : { width: 390, height: 844 },
      locale: language === 'en' ? 'en-US' : 'zh-CN', colorScheme: theme });
    await context.addInitScript(({ host, language, theme, pilotDeckToken, actorLogin, targetId }) => {
      if (host === 'pd') {
        localStorage.setItem('auth-token', pilotDeckToken);
        localStorage.setItem('userLanguage', language === 'en' ? 'en' : 'zh-CN');
        localStorage.setItem('themeMode', theme);
      } else {
        localStorage.setItem('ultrarag_auth', JSON.stringify(actorLogin));
        localStorage.setItem('staffdeck_locale', language === 'en' ? 'en-US' : 'zh-CN');
        localStorage.setItem('staffdeck_theme', theme);
        localStorage.setItem('ultrarag_enterprise_agent_scope', targetId);
        localStorage.setItem('staffdeck_onboarding_guide_seen', '1');
        localStorage.setItem('staffdeck_quick_start_guide_seen', '1');
      }
    }, { host, language, theme, pilotDeckToken: pilotDeckLogin.token, actorLogin, targetId: target.id });
    const page = await context.newPage();
    page.setDefaultTimeout(30_000);
    const row = { id, host, language, theme, viewport, routes: [], result: 'FAIL' };
    try {
      const origin = host === 'pd' ? pilotDeckOrigin : staffDeckOrigin;
      for (const path of routes[host]) {
        await page.goto(new URL(path, origin).href);
        await page.locator('body').waitFor({ state: 'visible' });
        if (path === routes[host][0]) await page.getByText(knowledgeTitle, { exact: true }).filter({ visible: true }).first().waitFor();
        else if (path.includes('/distill')) {
          await page.getByRole('button', { name: /显示源码|Show source/i }).filter({ visible: true }).first().click();
          await page.getByText(/^(Node status|节点状态)$/i).filter({ visible: true }).first().waitFor({ timeout: 4000 }).catch(() => {});
        }
        else await page.waitForTimeout(800);
        const route = { path, title: await page.title(), bodyVisible: Boolean((await page.locator('body').innerText()).trim()) };
        if (path.includes('/distill')) {
          const status = page.getByText(/^(Node status|节点状态)$/i).filter({ visible: true }).first();
          route.nodeStatus = await status.count() ? await status.locator('..').innerText() : null;
          route.staticStatusLocalized = language === 'en' ? Boolean(route.nodeStatus && !/起始节点|必选|流程节点|终止节点/.test(route.nodeStatus)) : Boolean(route.nodeStatus);
        }
        await page.screenshot({ path: `${root}/${id}-${path.includes('/distill') ? 'distill' : path.endsWith('/new') ? 'knowledge-new' : path.includes('skills') || path.endsWith('/sop') ? 'sop' : 'knowledge'}.png`, fullPage: true });
        row.routes.push(route);
      }
      row.themeApplied = await page.evaluate(() => document.documentElement.classList.contains('dark')) === (theme === 'dark');
      if (host === 'sd') row.themeSwitchVisible = await page.getByRole('button', { name: /切换到浅色主题|切换到深色主题|Switch to light theme|Switch to dark theme/i }).count() > 0;
      await page.goto(new URL(routes[host][0], origin).href);
      const menu = page.getByRole('button', { name: /知识库操作|Knowledge base actions/i }).filter({ visible: true }).first();
      await menu.click();
      await page.getByRole('menuitem', { name: /详情|Details/ }).first().click();
      const dialog = page.getByRole('dialog').last();
      await dialog.waitFor({ state: 'visible' });
      row.details = await dialog.evaluate(node => {
        const field = node.querySelector('textarea');
        const style = field ? getComputedStyle(field) : null;
        return { visible: true, textarea: Boolean(field), color: style?.color, background: style?.backgroundColor, textFill: style?.webkitTextFillColor,
          userContent: Boolean(field?.value.trim()) };
      });
      await page.screenshot({ path: `${root}/${id}-details.png`, fullPage: true });
      await dialog.getByRole('button', { name: /^(Close|关闭)$/ }).first().click();
      await dialog.waitFor({ state: 'hidden' });
      row.details.semanticClose = true;
      row.result = row.routes.every(route => route.bodyVisible && (route.staticStatusLocalized ?? true)) && row.themeApplied && (host === 'pd' || row.themeSwitchVisible)
        && row.details.userContent && row.details.semanticClose ? 'PASS' : 'FAIL';
    } catch (error) {
      row.result = 'BLOCKED';
      row.reason = error instanceof Error ? error.message.split('\n')[0] : String(error);
      await page.screenshot({ path: `${root}/${id}-error.png` }).catch(() => {});
    } finally {
      rows.push(row);
      await writeFile(`${root}/matrix.json`, JSON.stringify(rows, null, 2));
      console.log(id, row.result);
      await context.close();
    }
  }
} finally { await browser.close(); }
if (rows.some(row => row.result !== 'PASS')) process.exitCode = 1;
