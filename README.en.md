# osu-map-cleaner

English | [简体中文](README.md)

Batch-clean osu! beatmaps by a filter expression: matched difficulty files are moved to the system recycle bin (recoverable); whole sets can be removed as well.

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
| `star`             | nomod star rating (taken from the difficulty's own mode; backfill with `--calc-star` when osu! never calculated one) |
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

# A set is only removed entirely when every difficulty is below 2 stars
osu-map-cleaner "star<2" --match all

# Remove whole set folders as soon as any difficulty matches
osu-map-cleaner "key=7 star<3" --target set

# Backfill star ratings osu! never wrote (bulk-imported std maps) with rosu-pp, then filter
osu-map-cleaner "star<3" --calc-star --dry-run
```

Options:

| Option                    | Description                                                                                                            |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `-d, --dir <DIR>`         | osu! game directory (containing `osu!.db` and `Songs/`); defaults to current directory                                 |
| `-s, --sample <N>`        | Number of examples shown in the preview (default 5)                                                                    |
| `-m, --match <any\|all>`  | Set matching logic: any difficulty matches / all difficulties match (default any)                                      |
| `-t, --target <set\|map>` | Deletion granularity: only matched difficulty files (default; emptied folders are removed as well) / whole set folders |
| `--dry-run`               | Preview only, delete nothing                                                                                           |
| `--calc-star`             | Backfill missing star ratings offline with rosu-pp (lazer algorithm; measured deviation from db values < 0.5 stars)    |

## Safety design

- Deleted difficulty files / set folders are moved to the **system recycle bin** and can be restored at any time.
- Before deletion, examples and the total count are listed and a `Y/n` confirmation is required; if the operation would wipe **every** set on disk, you must type the full word `yes`.
- Folder and file names from the db are accepted only as plain relative paths inside `Songs/` (`..`, absolute paths, drive letters, and UNC paths are rejected), and each set path is canonicalized for a second check before deletion.
- Per-difficulty deletion (the default) verifies each file against the db's MD5 record; files with mismatching content or unreadable files are always skipped, and a folder is only removed once no `.osu` difficulty is left on disk.
- `osu!.db` parsing is strictly fail-loud (the 2026 db format is supported; star ratings are stored as f32). Unparseable star ratings count as non-matching — unknown data never causes a deletion.
- `--calc-star` accepts only positive star values: empty files, corrupt maps, and maps rosu-pp flags as suspicious stay "unknown", which star conditions never match.
