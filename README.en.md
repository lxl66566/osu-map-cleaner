# osu-map-cleaner

English | [简体中文](README.md)

Batch-clean osu! beatmap sets by a filter expression: matched sets are moved to the system recycle bin (recoverable).

## Usage

```
osu-map-cleaner [OPTIONS] <EXPR>...
```

The expression is a space-separated list of conditions, all of which must match (AND), e.g. `"key=7 star<3"`.

<!-- prettier-ignore -->
| Field              | Meaning                                                                 |
| ------------------ | ----------------------------------------------------------------------- |
| `key`              | mania key count (= CS; only mania difficulties can match)               |
| `cs`               | circle size                                                             |
| `star`             | nomod star rating (taken from the difficulty's own mode)                |
| `ar` / `od` / `hp` | AR / OD / HP                                                            |
| `length` / `drain` | total length / drain time (seconds)                                     |
| `mode`             | `std` / `taiko` / `ctb`(fruits) / `mania`, or 0-3                       |
| `status`           | `unknown` / `unsubmitted` / `pending` / `ranked` / `approved` / `qualified` / `loved` |

Operators: `=` `==` `!=` `<` `<=` `>` `>=` (`mode` / `status` only support `=` `!=`).

```
# Preview first, delete nothing
osu-map-cleaner "key=7 star<3" -d C:\game\osu --dry-run

# Move to the recycle bin after confirming (the expression may also be split into multiple args)
osu-map-cleaner key=7 star<3 -d C:\game\osu

# Only delete sets whose every difficulty is below 2 stars
osu-map-cleaner "star<2" --match all
```

Options:

| Option                   | Description                                                                            |
| ------------------------ | -------------------------------------------------------------------------------------- |
| `-d, --dir <DIR>`        | osu! game directory (containing `osu!.db` and `Songs/`); defaults to current directory |
| `-s, --sample <N>`       | Number of examples shown in the preview (default 5)                                    |
| `-m, --match <any\|all>` | Set matching logic: any difficulty matches / all difficulties match (default any)      |
| `--dry-run`              | Preview only, delete nothing                                                           |

## Safety design

- Matched beatmap sets are moved to the **system recycle bin** and can be restored at any time.
- Before deletion, examples and the total count are listed and a `Y/n` confirmation is required; if the expression matches **every** set on disk, you must type the full word `yes`.
- Folder names from the db are accepted only as plain relative paths inside `Songs/` (`..`, absolute paths, drive letters, and UNC paths are rejected), and each path is canonicalized for a second check before deletion.
- `osu!.db` parsing is strictly fail-loud (the 2026 db format is supported; star ratings are stored as f32). Unparseable star ratings count as non-matching — unknown data never causes a deletion.
