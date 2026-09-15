# osu-map-cleaner

简体中文 | [English](README.en.md)

按过滤表达式批量清理 osu! 谱面：默认仅将匹配的难度文件移动到系统回收站（可恢复），也可选择整组删除谱面集。

## 用法

```
osu-map-cleaner [OPTIONS] <EXPR>...
```

表达式为空格分隔的多个条件，全部满足才匹配（AND），例如 `"key=7 star<3"`。

<!-- prettier-ignore -->
| 字段    | 含义                                            |
| ------- | ----------------------------------------------- |
| `key`   | mania 键数（= CS，仅 mania 难度可命中）          |
| `cs`    | 圆圈大小                                        |
| `star`  | nomod 星数（取难度自身模式的评分）               |
| `ar` / `od` / `hp` | AR / OD / HP                         |
| `length` / `drain` | 总时长 / 排血时长（秒）              |
| `mode`  | `std` / `taiko` / `ctb`(fruits) / `mania` 或 0-3 |
| `status`| `unknown` / `unsubmitted` / `pending` / `ranked` / `approved` / `qualified` / `loved` |

运算符：`=` `==` `!=` `<` `<=` `>` `>=`（`mode` / `status` 仅支持 `=` `!=`）。

```
# 先预览，不删除
osu-map-cleaner "key=7 star<3" -d C:\game\osu --dry-run

# 确认后移动到回收站（表达式也可拆成多个参数）
osu-map-cleaner key=7 star<3 -d C:\game\osu

# 任一难度匹配即整组移除谱面集目录
osu-map-cleaner "key=7 star<3" --target set

# 全部难度都在 2 星以下时，该谱面集才会整组移除
osu-map-cleaner "star<2" --match all
```

选项：

| 选项                      | 说明                                                                   |
| ------------------------- | ---------------------------------------------------------------------- |
| `-d, --dir <DIR>`         | osu! 游戏目录（含 `osu!.db` 与 `Songs/`），默认当前目录                |
| `-s, --sample <N>`        | 预览时显示的示例数量（默认 5）                                         |
| `-m, --match <any\|all>`  | 谱面集匹配逻辑：任一难度匹配 / 全部难度匹配（默认 any）                |
| `-t, --target <set\|map>` | 删除粒度：仅匹配的难度文件（默认，清空的目录一并移除）/ 整个谱面集目录 |
| `--dry-run`               | 仅预览，不删除                                                         |

## 安全设计

- 删除的难度文件 / 谱面集目录移动到**系统回收站**，可随时恢复。
- 删除前列出示例与总数，必须通过 `Y/n` 确认；若会清空磁盘上**全部**谱面集，需完整输入 `yes`。
- db 中的目录名与文件名仅接受 `Songs/` 内的纯相对路径（拒绝 `..`、绝对路径、盘符、UNC），删除前还会 canonicalize 二次校验。
- 按难度删除（默认）时逐文件校验 db 记录的 MD5，内容不符或无法读取的文件一律跳过；目录只有在磁盘上已无任何 `.osu` 难度时才会移除。
- `osu!.db` 解析严格 fail-loud（已支持 2026 版 db，星数为 f32 存储）；无法解析的星数按不匹配处理，绝不因未知数据误删。
