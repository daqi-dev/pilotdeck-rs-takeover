# Web16 联调组合与构建说明

本分支保存 Web16 原始候选 `e212384ed0c8573e52da88e8a95a8be4d6982ef6` 的 Rust 源码和联调 overlay，并纳入固定基线 `OpenBMB/PilotDeck@cc7c4e653b15c8b2cec7bcf2d15d69396ae85` 的 TypeScript 源码快照，位于 `vendor/pilotdeck-ts/`。Rust `src` 树为 `8fd385fa690b4ec6128bba21272825921cc77db9`，与 backend62 相同；许可证保留在源码目录。原始源码的前端 patch 由 `scripts/backend-web-integration/patches.json` 按构建期应用。快照清单位于 `scripts/backend-web-integration/vendored-baseline.json`，构建前后逐文件校验 SHA-256。快照 `.gitattributes` 为避免隐式 LFS 操作而追加禁用过滤器的规则；前端 UI 需要的 11 个 LFS 图片已按原始 LFS SHA-256 收录为普通 Git 文件。其他上游 LFS 资源仍为指针，不属于本次网页构建验证范围。

## 构建

要求 Node `>=22.13.0 <23`、与 `vendor/pilotdeck-ts/pnpm-lock.yaml` 相容的依赖。首次准备依赖可在 vendored 目录依据锁文件运行 `pnpm install --frozen-lockfile`；依赖包可能需要访问包仓库。源码无需再克隆 PilotDeck 上游仓库。依赖安装过程与原版系统依赖（包括可能的原生构建工具）由目标环境提供。

```sh
cd vendor/pilotdeck-ts
pnpm install --frozen-lockfile
cd ../..
node scripts/backend-web-integration/build.mjs --baseline "$PWD/vendor/pilotdeck-ts" --out /absolute/path/to/new-output
```

输出目录必须不存在，且不与源码目录重叠。`dist/` 是网页构建产物；运行时仍需要按 `scripts/backend-web-integration/runtime/README.md` 连接 TS UI server 与 Rust gateway。此说明不替代运行环境隔离和鉴权配置。

## 实际验证边界

2026-11-15 在 macOS arm64、Node 22.22.0 使用独立临时源码副本及复制的本机依赖构建成功；通过系统沙箱禁止读取原 PilotDeck checkout，结果的 `index.html` SHA-256 为 `5915043744062e681bffe62161a7e927e20cd953ff0a7fca3201cd4b838f23ee`，与历史 Web16 记录相同。篡改清单内文件的负向控制被构建器拒绝。该次没有在全新机器从包仓库安装依赖，也没有重新执行端到端网页验收。

历史 backend62 + Web16 高风险矩阵为 34 PASS、1 PARTIAL、1 NOT_RUN；真实 LLM 另账 PARTIAL。历史结果不能证明当前改装后的快照已经完成相同端到端验收。独立构建不代表 ifACE 已接通，也不代表未合入的新前端已验收。

安全检查：原 backend62 全历史的 gitleaks 134 个命中来自测试及证据文件的 SHA-256（132）和固定 WebSocket 测试握手字段（2）；原 Web16 相对 backend62 的增量无命中。vendored TypeScript 源码快照的 3 条命中分别是测试会话标识、合成环境变量名和权限模式文案；新增源码校验清单中的 47 条命中是文件路径与 SHA-256 摘要的误报。保留这些原始内容，未改写测试证据。
