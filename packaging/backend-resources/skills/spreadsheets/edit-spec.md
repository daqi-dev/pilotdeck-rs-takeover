# 局部编辑 JSON

```json
{
  "sheets": [{
    "name": "Data",
    "cells": {
      "A1": {"value": "Revenue", "font": {"bold": true}, "fill": "DDEEFF"},
      "B2": {"value": 12, "numberFormat": "0.00"},
      "B4": {"formula": "=SUM(B2:B3)", "alignment": {"horizontal": "right"}}
    },
    "merges": ["D1:E1"],
    "columnWidths": {"A": 24},
    "rowHeights": {"1": 24},
    "freeze": "B2",
    "autoFilter": "A1:B3",
    "charts": [{"kind": "bar", "data": "B1:B3", "categories": "A2:A3", "title": "Amounts", "anchor": "E3"}]
  }],
  "names": {"Amounts": "'Data'!$B$2:$B$3"}
}
```

工作表必须已存在。每个单元格使用 `value` 或以 `=` 开头的 `formula`，不能同时提供。`value:null` 清空；日期需要任务脚本使用日期类型。`font` 与 `alignment` 使用安装的 openpyxl 对应构造参数。图表 `data` 首行是系列名称，`categories` 指向分类数据。规范不提供删除表、删除对象或批量重建模板的隐含操作。
