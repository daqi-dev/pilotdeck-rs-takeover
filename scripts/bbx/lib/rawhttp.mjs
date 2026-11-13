// raw 步骤（run.mjs）对原始 HTTP 应答的解析，抽出来供 rawhttp-selftest.mjs 自测。
// HTTP 应答按语义记：头名转小写、去掉 date；chunked 的正文解码后记录并标 chunked: true（chunk 长度的写法、
// 头名大小写都是 HTTP 语义等价的传输细节，不比）；同一连接上的后续应答（管线化）依次放进 next。
export const parseHttpHead = (raw) => {
  const i = raw.indexOf('\r\n\r\n'); const head = i < 0 ? raw : raw.slice(0, i);
  const [status, ...lines] = head.split('\r\n');
  const headers = Object.fromEntries(lines.map((l) => [l.slice(0, l.indexOf(':')).toLowerCase(), l.slice(l.indexOf(':') + 1).trim()]).filter(([k]) => k !== 'date'));
  return { status, headers, ...(i >= 0 && { body: raw.slice(i + 4) }) };
};
// raw 按 latin1 读入（一字符一字节，chunk 长度按字节数切），记录时再按 UTF-8 解码。
const utf8 = (x) => Buffer.from(x, 'latin1').toString('utf8');
export const parseHttpResponses = (raw) => {
  const first = parseHttpHead(raw);
  let rest = first.body;
  if (rest === undefined) return first;
  const te = first.headers['transfer-encoding']; const cl = first.headers['content-length'];
  if (te && /chunked/i.test(te)) {
    let body = ''; let at = 0; let complete = false;
    for (;;) {
      const eol = rest.indexOf('\r\n', at); if (eol < 0) break;
      const size = parseInt(rest.slice(at, eol), 16); if (!Number.isFinite(size)) break;
      if (size === 0) { const end = rest.indexOf('\r\n', eol + 2); at = end < 0 ? rest.length : end + 2; complete = end >= 0; break; }
      body += rest.slice(eol + 2, eol + 2 + size); at = eol + 2 + size + 2;
    }
    first.body = utf8(body); first.chunked = complete ? true : 'incomplete'; rest = rest.slice(at);
  } else if (cl !== undefined) {
    first.body = utf8(rest.slice(0, Number(cl))); rest = rest.slice(Number(cl));
  } else { first.body = utf8(rest); rest = ''; }
  if (rest.startsWith('HTTP/')) first.next = parseHttpResponses(rest);
  else if (rest) first.trailing = utf8(rest);
  return first;
};
