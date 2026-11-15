import React, { useMemo, useRef } from 'react';
import ReactMarkdown, { defaultUrlTransform } from 'react-markdown';
import { getActiveAssembly } from '../../../../composition/runtime';
import type { MarkdownLinkContribution } from '../../../../composition/contracts';
import type { Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import rehypeKatex from 'rehype-katex';
import { MarkdownCodeBlock, MarkdownTable, MarkdownSourceContext } from './MarkdownCopyBlocks';
import { resolveMarkdownFileHref } from '../../utils/resolveMarkdownFileHref';
import {
  createRemarkArtifactFileTextPlugin,
  type MarkdownArtifactFile,
} from '../../utils/remarkArtifactFileText';

type MarkdownProps = {
  children: React.ReactNode;
  className?: string;
  projectName?: string;
  isStreaming?: boolean;
  onFileOpen?: (filePath: string) => void;
  artifactFiles?: MarkdownArtifactFile[];
};

const fullRehypePlugins = [rehypeKatex];

const linkClassName = 'text-blue-600 hover:underline dark:text-blue-400';

function createMarkdownComponents(onFileOpen: ((filePath: string) => void) | undefined, links: MarkdownLinkContribution[]): Components {
  return {
    pre: MarkdownCodeBlock,
    table: MarkdownTable,
    a: ({ href, children, ...props }) => {
      const renderer = links.find(link => href?.startsWith(`${link.protocol}//`));
      if (renderer && href) {
        const Link = renderer.component;
        return <Link href={href}>{children}</Link>;
      }
      const filePath = resolveMarkdownFileHref(href);
      if (filePath && onFileOpen) {
        return (
          <a
            href={href}
            className={`${linkClassName} cursor-pointer`}
            onClick={(event) => {
              event.preventDefault();
              onFileOpen(filePath);
            }}
            {...props}
          >
            {children}
          </a>
        );
      }

      const isExternal = Boolean(href && /^https?:\/\//i.test(href));
      return (
        <a
          href={href}
          className={linkClassName}
          {...(isExternal ? { target: '_blank', rel: 'noopener noreferrer' } : {})}
          {...props}
        >
          {children}
        </a>
      );
    },
  };
}

export const Markdown = React.memo(function Markdown({
  children,
  className,
  isStreaming,
  onFileOpen,
  artifactFiles,
}: MarkdownProps) {
  const content = String(children ?? '');

  const links = getActiveAssembly()?.markdownLinkRenderers ?? [];
  const components = useMemo(
    () => createMarkdownComponents(onFileOpen, links),
    [onFileOpen, links],
  );
  const remarkPlugins = useMemo(() => {
    if (isStreaming) return [remarkGfm, remarkMath];
    if (artifactFiles === undefined) return [remarkGfm, remarkMath];
    return [remarkGfm, remarkMath, createRemarkArtifactFileTextPlugin(artifactFiles)];
  }, [artifactFiles, isStreaming]);

  // Only apply streaming-fade-in on the initial mount while streaming.
  // Once streaming ends, never re-apply it — prevents old content from
  // briefly re-animating when sibling messages cause a re-render.
  const wasStreamingRef = useRef(!!isStreaming);
  if (!isStreaming) wasStreamingRef.current = false;
  const showFadeIn = isStreaming && wasStreamingRef.current;

  return (
    <div className={`chat-markdown ${className || ''} ${showFadeIn ? 'streaming-fade-in' : ''}`.trim()}>
      <MarkdownSourceContext.Provider value={content}>
        <ReactMarkdown
          remarkPlugins={remarkPlugins}
          rehypePlugins={fullRehypePlugins}
          components={components}
          urlTransform={(url) => links.some(link => url.startsWith(`${link.protocol}//`)) ? url : defaultUrlTransform(url)}
        >
          {content}
        </ReactMarkdown>
      </MarkdownSourceContext.Provider>
    </div>
  );
});
