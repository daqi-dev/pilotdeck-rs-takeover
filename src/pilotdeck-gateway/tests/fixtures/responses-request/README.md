# OpenAI responses 请求体（规范请求 → 请求体）对拍

`oracle.json` 由基线 `cc7c4e6` 已构建的 `dist/src/model/providers/openai-responses/request.js` 里的 `buildOpenAIResponsesRequest` 直接算出，不起服务、不发网络请求。补的是 `../model-stream-responses/` 回放装置覆盖不到的分支：字段真缺省（无 `tools` / `metadata` / `systemPrompt` / `maxOutputTokens` / `stream`）、`tool_choice` 为 `none` / `required` / `auto`、历史消息的各种块、推理强度、`service_tier`、结构化输出。

重新生成（仓库根目录，隔离的 HOME）：

```sh
H="$(mktemp -d)"; HOME="$H" PILOT_HOME="$H/.pilotdeck" \
  node src/pilotdeck-gateway/tests/fixtures/responses-request/generate.mjs \
  /absolute/path/to/PilotDeck src/pilotdeck-gateway/tests/fixtures/responses-request/oracle.json
```

回放：`providers::openai_responses` 的单元测试 `build_request_matches_baseline_oracle`，逐例比较 `JSON.stringify` 后的文本（键序也比）。
