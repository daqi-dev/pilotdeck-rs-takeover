---
id: D1-MEMORY-SEALED-TREE-ORDER-001
---

## 类别

不可避免：仅两份固定基线TS原件的有限采集键序排列补充，不是产品行为差异或全局对象排序许可。

## 基线依据

固定 cc7c4e653b15c8b2cec7bcf2d15d41d69396ae85 的真实 `agent03-sdk-memory-lifecycle` 两TS采集；scene SHA d654c045d164826de415eb7581325471b5b0b32b57d664a8069a1feb504e101a。两raw SHA分别 cd96411ddc477083a32947bf4d28cc999675b6fd3431f1f5b39efd5fe020c75e 与 5ca79f49465369b96c9b1586018b6927661818bbaaf06f652d6ba9739fd609fd。原644正式self仅 `$.tree.<key order>` DIFF1；完整命令/身份/输入SHA见 `_tmp/20261113-abc-formal-644a8d80-final/result.json`（SHA a3b4aa0efe32f3edcdfdea5b6b2a47ea92c95746291487b137f565ed681ae358），原成绩保留。公共fixture保存原字节和此closure回执；不是根据文档推断行为。

## Rust 行为

此补充不改变Rust产品或TS/Rust pair，不授予Rust字段差异。仅公共比较器self入口在固定原件、完整lifecycle重算后打印单叶许可。

## 范围

只这两份sealed TS原件且固定左右顺序。27个tree键中0基索引5–8，两侧分别是worker transcript/projection后接reviewer transcript/projection、以及两个整组反序。组内序、其余23键名称/位置/顺序、全部内容/正文/归属/ID与原既有全链见证保持严格。仅许可 `$.tree.<key order>`；pair、未来capture、其他tree/scene/排列不适用。不整字段volatile、不重排array或record。

## 机器判据

`scripts/bbx/memory-tree-order-policy.mjs` 内置两raw SHA和closure原/压缩SHA，先核压缩字节再有界解压，固定原644生命周期实现SHA并重算完整见证，不接受外部passed/view/Map或可替换manifest。未知原件返回空许可，仍走普通比较；固定证据损坏拒绝。`node --test scripts/bbx/memory-tree-order-policy.test.mjs` 真实公共CLI对原件self从DIFF1变为SAME，非self仍DIFF1，并检查内容/键名/缺失/外scope/其他键序/归属/未观察排列/错side/交换原件/篡改closure等。旧FAIL及原件不覆盖；不涉及产品重采、Cargo或网络。

## 批准记录

2026-11-14，本会话用户明确批准 memory 两固定原件精确self补充；主协调者授权正式接线。批准只针对私有已审 manifest SHA d5fffbaa1175c9d2c9d1fb4eb27b699fdcd47bdf6e72fe82465a07680c08a799、candidate SHA 680d02526f36ddd749f32b38d2e4e863658dec8bfd3364c8b388597176cd594a 的范围。S1非作者复审独立12/12无skip且无新增阻断；正式接线仍须SDK独审与冻结重登记。state/decisions由主协调者独占记录，本条不改任何冻结aggregate/FROZEN，不将原A六范围自动扩大。
