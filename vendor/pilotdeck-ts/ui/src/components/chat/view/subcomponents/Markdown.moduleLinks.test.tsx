// @vitest-environment jsdom
import { cleanup, render, screen } from '@testing-library/react';
import { afterEach, expect, it, vi } from 'vitest';
import type { MarkdownLinkContribution } from '../../../../composition/contracts';
const state = vi.hoisted(() => ({ links: [] as MarkdownLinkContribution[] }));
vi.mock('../../../../composition/runtime', () => ({ getActiveAssembly: () => ({ markdownLinkRenderers: state.links }) }));
import { Markdown } from './Markdown';
afterEach(() => { cleanup(); state.links = []; });
it('renders selected module links and keeps the normal sanitizer for unselected schemes', () => {
  state.links = [{ id: 'source', protocol: 'ultrarag:', component: ({ href, children }) => <a href={href} data-testid="source">{children}</a> }];
  const { rerender } = render(<Markdown>{'[Original](ultrarag://knowledge/documents/exact) [Unsafe](javascript:alert%281%29)'}</Markdown>);
  expect(screen.getByTestId('source').getAttribute('href')).toBe('ultrarag://knowledge/documents/exact');
  expect(screen.getByText('Unsafe').getAttribute('href')).toBe('');
  state.links = [];
  rerender(<Markdown>{'[Original](ultrarag://knowledge/documents/exact)'}</Markdown>);
  expect(screen.getByText('Original').getAttribute('href')).toBe('');
});
