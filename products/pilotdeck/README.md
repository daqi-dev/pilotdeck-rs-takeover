# products/pilotdeck —— 产品仓源码闭包配方

> `pilotdeck-rs/pilotdeck`（`kind = "preset"`，v0.1.0）：把 pilotdeck-rs 全部
> 20 个 MintHub 源码 bundle 的**根集**（8 个入度 0 的包）精确钉进一条配方，
> 传递闭包覆盖其余 12 个包。本包不含实现、不执行代码、不声明权限，只有
> `[dependencies]`。

## 为什么在产品仓（而不是 agent-kernel 仓）

两级门（`/Users/da/Downloads/package-release-gates.md` §2.1 自查项 6）要求产品包
「在 products/* 配方内被引用」。agent-kernel 仓的 `products/pilotdeck`
（`agentcpm/pilotdeck-dev`）是 Kernel+PD 声明包桥接配方，不含 `pilotdeck-rs/*`
依赖（见该文件第四节第 7 条明确排除）；本产品仓的这条配方补上等价引用。
口径与证据：

- 根集计算复用 `scripts/verify-minthub-stitched-assembly.sh` 的 online-roots
  算法（对 20 个 `packages/minthub/*/mint.toml` 依赖入度求 0）；
- 端到端可复现：`bash scripts/verify-minthub-stitched-assembly.sh --mint <mint> --jobs 6 --out-report-dir <dir>`（双平台 4 阶段全绿，见
  `docs/iface-integration/minthub-stitched-verification.md`）。

## 使用

```bash
mint init my-pd -y --no-preset
cd my-pd
mint --registry https://mint-hub.ea.modelbest.co add -y "path:../products/pilotdeck"
mint list   # 应看到 20 个 pilotdeck-rs/* 包（8 根 + 12 传递）
```

## 维护

- 任何 `pilotdeck-rs/*` 升版 ⇒ 重算根集（入度可能变化）→ 改本目录 `mint.toml`
  的对应钉 → preset `version` +1；
- 本配方不被 `scripts/publish-pilotdeck-mint-hub.sh` 管理（该脚本强制
  `packages/minthub` 恰为 20 个 bundle 目录）；如需发布到 Hub，手动
  `mint pack` + `mint publish` 并在 wave 报告留痕。
