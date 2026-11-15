// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import { authenticatedFetch } from '../../../utils/api';
import { ApproverAccount } from './ApproverAccount';
import { useApproverSession } from './useApproverSession';
vi.mock('../../../utils/api', () => ({ authenticatedFetch: vi.fn() }));
vi.mock('react-i18next', () => ({ useTranslation: () => ({ t: (key: string) => key }) }));
afterEach(() => { cleanup(); vi.resetAllMocks(); });
function Account() { return <ApproverAccount session={useApproverSession('owner')} />; }
it('exposes normal StaffDeck login and logout through the stock account dialog', async () => {
  vi.mocked(authenticatedFetch).mockResolvedValueOnce(new Response(JSON.stringify({ token: 'synthetic-normal-token',
    user: { id: 'approver', username: 'Approver', tenant_id: 'tenant' } })));
  render(<Account />);
  fireEvent.click(screen.getByRole('button', { name: 'login.submit' }));
  fireEvent.change(screen.getByLabelText('login.username'), { target: { value: 'Approver' } });
  fireEvent.change(screen.getByLabelText('login.password'), { target: { value: 'synthetic-password' } });
  fireEvent.submit(screen.getByLabelText('login.password').closest('form')!);
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(screen.getByText('StaffDeck · Approver')).toBeTruthy();
  fireEvent.click(screen.getByRole('button', { name: 'logout.button' }));
  expect(screen.getByRole('button', { name: 'login.submit' })).toBeTruthy();
});
