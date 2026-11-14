import { createElement, useState } from 'react';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import ProviderCard from '@pilotdeck-baseline/ui/src/components/settings/view/modelPool/components/ProviderCard';
import ModelsSection from '@pilotdeck-baseline/ui/src/components/settings/view/modelPool/components/ModelsSection';
import { maskSecrets, normalizePilotDeckConfig, preserveMaskedSecrets } from '@pilotdeck-baseline/ui/server/services/pilotdeckConfig.js';

const mocks = vi.hoisted(() => ({ fetch: vi.fn(), models: vi.fn() }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
vi.mock('@pilotdeck-baseline/ui/src/utils/api', () => ({ authenticatedFetch: mocks.fetch }));
vi.mock('@pilotdeck-baseline/ui/src/shared/modelListApi', () => ({ fetchProviderModels: mocks.models }));
const defaultUrl = 'https://api.z.ai/api/paas/v4';
// Synthetic local fixture only. No user state or provider requests are read.
const alternateUrl = 'https://coding-plan.example.test/api/coding/paas/v4';
const baseLabel = 'pilotDeckConfig.panels.models.baseUrl';
const keyLabel = 'pilotDeckConfig.panels.models.apiKey';
const saveLabel = 'actions.saveChanges';
const editLabel = 'settingsPage.actions.edit';
const syntheticKey = 'synthetic-provider-control';
beforeEach(() => {
  mocks.fetch.mockReset(); mocks.models.mockReset();
  mocks.fetch.mockResolvedValue({ ok: true, json: async () => ({ tasks: [] }) });
  mocks.models.mockResolvedValue([]);
});
afterEach(cleanup);

it('saves a preset alternate URL through ModelsSection, rereads it, and preserves the masked key on unchanged-scope resave', async () => {
  let stored: any = { agent: { model: 'zhipu/glm-4.7' }, model: { providers: {
    zhipu: { protocol: 'openai', url: defaultUrl, apiKey: syntheticKey, models: { 'glm-4.7': {} } },
  } } };
  const writes: any[] = [];
  function Fixture() {
    const [config, setConfig] = useState(() => maskSecrets(stored));
    return createElement(ModelsSection, { config, onChange: async (next: any) => {
      writes.push(next);
      stored = normalizePilotDeckConfig(preserveMaskedSecrets(next, stored));
      setConfig(maskSecrets(stored));
      return { ok: true };
    } });
  }
  render(createElement(Fixture));
  fireEvent.click(await screen.findByRole('button', { name: editLabel }));
  expect((screen.getByLabelText(baseLabel) as HTMLInputElement).value).toBe(defaultUrl);
  expect(screen.queryByLabelText('pilotDeckConfig.panels.models.protocol')).toBeNull();
  fireEvent.change(screen.getByLabelText(baseLabel), { target: { value: alternateUrl } });
  // A scope change uses an explicitly entered synthetic key, matching the
  // existing server requirement; this test does not bypass masked-key scope.
  fireEvent.change(screen.getByLabelText(keyLabel), { target: { value: syntheticKey } });
  fireEvent.click(screen.getByRole('button', { name: saveLabel }));
  await waitFor(() => expect(writes).toHaveLength(1));
  expect(stored.model.providers.zhipu).toMatchObject({ url: alternateUrl, protocol: 'openai', apiKey: syntheticKey });
  fireEvent.click(await screen.findByRole('button', { name: editLabel }));
  expect((screen.getByLabelText(baseLabel) as HTMLInputElement).value).toBe(alternateUrl);
  expect((screen.getByLabelText(keyLabel) as HTMLInputElement).value).toBe('********');
  fireEvent.click(screen.getByRole('button', { name: saveLabel }));
  await waitFor(() => expect(writes).toHaveLength(2));
  expect(writes[1].model.providers.zhipu.apiKey).toBe('********');
  expect(stored.model.providers.zhipu).toMatchObject({ url: alternateUrl, protocol: 'openai', apiKey: syntheticKey });
  fireEvent.click(await screen.findByRole('button', { name: editLabel }));
  expect((screen.getByLabelText(baseLabel) as HTMLInputElement).value).toBe(alternateUrl);
  expect((screen.getByLabelText(keyLabel) as HTMLInputElement).value).toBe('********');
});

it('shows the unchanged catalog default when a preset has no explicit URL', async () => {
  const onSave = vi.fn(async () => ({ ok: true }));
  render(createElement(ProviderCard, { providerId: 'zhipu', initialEditing: true,
    provider: { protocol: 'openai', apiKey: '********', models: { 'glm-4.7': {} } }, onSave, onRemove: () => {} }));
  expect((screen.getByLabelText(baseLabel) as HTMLInputElement).value).toBe(defaultUrl);
  expect(screen.queryByLabelText('pilotDeckConfig.panels.models.protocol')).toBeNull();
  fireEvent.click(screen.getByRole('button', { name: saveLabel }));
  await waitFor(() => expect(onSave).toHaveBeenCalledWith('zhipu', expect.objectContaining({ url: defaultUrl, protocol: 'openai', apiKey: '********' })));
});

it('rejects invalid preset URLs before save and leaves the masked key untouched', () => {
  const onSave = vi.fn();
  render(createElement(ProviderCard, { providerId: 'zhipu', initialEditing: true,
    provider: { protocol: 'openai', url: defaultUrl, apiKey: '********', models: { 'glm-4.7': {} } }, onSave, onRemove: () => {} }));
  fireEvent.change(screen.getByLabelText(baseLabel), { target: { value: 'file:///invalid' } });
  fireEvent.click(screen.getByRole('button', { name: saveLabel }));
  expect(screen.getByText('pilotDeckConfig.panels.models.providerUrlInvalid')).toBeTruthy();
  expect(onSave).not.toHaveBeenCalled();
  expect((screen.getByLabelText(keyLabel) as HTMLInputElement).value).toBe('********');
});

it('keeps the existing custom provider protocol and URL inputs available', async () => {
  const onSave = vi.fn(async () => ({ ok: true }));
  render(createElement(ProviderCard, { providerId: 'custom_control', initialEditing: true,
    provider: { protocol: 'anthropic', url: 'https://custom.example.test', apiKey: '********', models: { model: {} } }, onSave, onRemove: () => {} }));
  expect((screen.getByLabelText('pilotDeckConfig.panels.models.protocol') as HTMLSelectElement).value).toBe('anthropic');
  fireEvent.change(screen.getByLabelText(baseLabel), { target: { value: 'https://custom.example.test/new' } });
  fireEvent.click(screen.getByRole('button', { name: saveLabel }));
  await waitFor(() => expect(onSave).toHaveBeenCalledWith('custom_control', expect.objectContaining({ url: 'https://custom.example.test/new', protocol: 'anthropic', apiKey: '********' })));
});
