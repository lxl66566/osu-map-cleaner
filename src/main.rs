//! CLI: filter osu! beatmap sets by expression and move them — or just
//! their matched difficulties — to the recycle bin.

use std::{
    collections::HashSet,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf, absolute},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hex_simd::{AsciiCase, encode_to_string};
use md5::{Digest, Md5};
use osu_map_cleaner::{
    clean::{self, BeatmapSet, MatchMode, TargetMode},
    expr::{Expr, NumField},
    stars,
};

#[derive(Parser)]
#[command(
    version,
    about = "按过滤表达式批量清理 osu! 谱面（默认仅删除匹配的难度，移动到回收站，可恢复）",
    after_help = "示例:\n  osu-map-cleaner \"key=7 star<3\"  (默认仅删除匹配的难度文件)\n  \
                  osu-map-cleaner key=7 star<3 -d C:\\game\\osu --dry-run\n  osu-map-cleaner \
                  \"star<2 length<60 mode=mania\" --match all\n  osu-map-cleaner \"key=7\" \
                  --target set  (任一难度匹配则整组移除)"
)]
struct Args {
    /// 过滤表达式，如 "key=7 star<3"（也可拆成多个参数）
    #[arg(required = true, num_args = 1..)]
    expr: Vec<String>,

    /// osu! 游戏目录（需包含 osu!.db 与 Songs/），默认当前目录
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,

    /// 预览时显示的示例数量
    #[arg(short = 's', long, default_value_t = 5)]
    sample: usize,

    /// 谱面集匹配逻辑: any = 任一难度匹配即选中, all = 全部难度匹配才选中
    #[arg(short = 'm', long = "match", value_enum, default_value_t = CliMatchMode::Any)]
    match_mode: CliMatchMode,

    /// 删除粒度: map = 仅匹配的难度文件（默认）, set = 整个谱面集目录
    #[arg(short = 't', long, value_enum, default_value_t = CliTarget::Map)]
    target: CliTarget,

    /// 仅预览匹配结果，不删除
    #[arg(long)]
    dry_run: bool,

    /// 用 rosu-pp 本地补算缺失的星数（osu! 对批量导入的谱面不写星数）
    #[arg(long)]
    calc_star: bool,
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum CliMatchMode {
    Any,
    All,
}

impl From<CliMatchMode> for MatchMode {
    fn from(v: CliMatchMode) -> Self {
        match v {
            CliMatchMode::Any => Self::Any,
            CliMatchMode::All => Self::All,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum CliTarget {
    Set,
    Map,
}

impl From<CliTarget> for TargetMode {
    fn from(v: CliTarget) -> Self {
        match v {
            CliTarget::Set => Self::Set,
            CliTarget::Map => Self::Map,
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("错误: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let expr = Expr::parse(&args.expr.join(" ")).context("解析表达式失败")?;
    let match_mode = MatchMode::from(args.match_mode);
    let target_mode = TargetMode::from(args.target);

    let dir = absolute(&args.dir).with_context(|| format!("目录不可用: {}", args.dir.display()))?;
    let db_path = dir.join("osu!.db");
    let songs_dir = dir.join("Songs");
    if !db_path.is_file() {
        bail!("未找到 {}", db_path.display());
    }
    if !songs_dir.is_dir() {
        bail!("未找到 Songs 目录: {}", songs_dir.display());
    }

    println!("osu! 目录 : {}", dir.display());
    println!("数据库   : {}", db_path.display());
    println!(
        "表达式   : {expr}  ({}, {})",
        match_mode.label(),
        target_mode.label()
    );

    let db_bytes = std::fs::read(&db_path).with_context(|| {
        format!(
            "读取 {} 失败（osu! 正在运行？权限不足？）",
            db_path.display()
        )
    })?;
    let db = osu_map_cleaner::db::parse(&db_bytes)
        .with_context(|| format!("解析 {} 失败（db 版本不受支持？）", db_path.display()))?;
    let (mut sets, skipped) = clean::group_sets(&db.beatmaps);
    println!(
        "共 {} 张谱面, {} 个谱面集 (db 版本 {}{})",
        db.beatmaps.len(),
        sets.len(),
        db.version,
        if skipped > 0 {
            format!(", {skipped} 张缺少目录名已跳过")
        } else {
            String::new()
        }
    );

    let unknown_stars = sets
        .iter()
        .flat_map(|s| &s.maps)
        .filter(|d| d.info.star.is_none())
        .count();
    if args.calc_star {
        println!("补算星数: {unknown_stars} 个难度缺失（rosu-pp, lazer 算法, 离线并行）…");
        let stats = stars::fill_missing_stars(&mut sets, &songs_dir);
        println!(
            "星数补算完成: 成功 {} | 文件缺失 {} | 无文件名 {} | 路径异常 {} | 计算失败 {}",
            stats.computed,
            stats.missing_file,
            stats.no_file_name,
            stats.unsafe_name,
            stats.calc_failed
        );
        if stats.computed > 0 {
            println!("提示: 补算值为 osu!lazer 算法，与 osu! 显示可能有细微偏差（实测 <0.5 星）");
        }
    } else if unknown_stars > 0 && expr.uses_num_field(NumField::Star) {
        println!(
            "提示: {unknown_stars} 个难度星数未知（osu! 未计算），star 条件不会匹配它们；加 \
             --calc-star 可本地补算"
        );
    }

    // Resolve folders for all sets (not only matched ones) so the
    // "matched everything?" guard counts what actually exists on disk.
    let mut targets = Vec::new();
    let mut existing = 0usize;
    let mut missing = 0usize;
    let mut unsafe_names = 0usize;
    let mut file_skips = FileSkips::default();
    let songs_canon = songs_dir.canonicalize().context("无法访问 Songs 目录")?;
    for set in &sets {
        let matched = set.matches(&expr, match_mode);
        if !clean::is_safe_rel_path(&set.folder) {
            unsafe_names += usize::from(matched);
            continue;
        }
        // is_safe_rel_path rejects `..` and absolute prefixes, so a
        // canonical path under Songs/ cannot escape it.
        let path = songs_dir.join(&set.folder);
        let on_disk = path.is_dir()
            && path.canonicalize().is_ok_and(|c| {
                c.strip_prefix(&songs_canon)
                    .is_ok_and(|rest| !rest.as_os_str().is_empty())
            });
        if !on_disk {
            missing += usize::from(matched);
            continue;
        }
        existing += 1;
        if !matched {
            continue;
        }
        let action = match target_mode {
            TargetMode::Set => Action::RemoveSet {
                size: clean::dir_size(&path),
            },
            TargetMode::Map => {
                let Some(action) = plan_map_removal(set, &path, &expr, &mut file_skips) else {
                    continue;
                };
                action
            },
        };
        targets.push(Target { set, path, action });
    }

    print_preview(
        &targets,
        existing,
        missing,
        unsafe_names,
        &file_skips,
        target_mode,
        &expr,
        &args,
    );

    if targets.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        println!("--dry-run: 仅预览，未删除任何内容。");
        return Ok(());
    }

    // Removing every set on disk is almost certainly a mistake; require the
    // full word instead of a single keypress. In map mode this means every
    // set folder would end up emptied.
    let removes_all =
        targets.len() == existing && targets.iter().all(|t| t.action.removes_folder());
    let confirmed = if removes_all {
        println!("\n*** 警告: 本次操作将清空磁盘上全部 {existing} 个谱面集！***");
        prompt_strict()?
    } else {
        confirm("确认将以上内容移动到回收站?")?
    };
    if !confirmed {
        println!("已取消，未删除任何内容。");
        return Ok(());
    }
    delete_targets(&targets, &songs_dir);
    Ok(())
}

/// Per-file skips accumulated while planning a `--target map` deletion.
#[derive(Default)]
struct FileSkips {
    /// The .osu file does not exist on disk.
    missing: usize,
    /// The db entry carries no usable .osu file name.
    no_name: usize,
    /// File content does not match the db's md5, or the file is unreadable.
    verify_failed: usize,
}

/// One .osu file scheduled for removal.
struct FilePlan {
    path: PathBuf,
    size: u64,
}

/// What will be removed for one matched set.
enum Action {
    /// The whole set folder.
    RemoveSet { size: u64 },
    /// Individual .osu files; `empties` means every .osu in the folder is
    /// included, so the folder itself goes as well.
    RemoveFiles {
        files: Vec<FilePlan>,
        empties: bool,
        size: u64,
    },
}

impl Action {
    fn size(&self) -> u64 {
        match self {
            Self::RemoveSet { size } | Self::RemoveFiles { size, .. } => *size,
        }
    }

    fn file_count(&self) -> usize {
        match self {
            Self::RemoveSet { .. } => 0,
            Self::RemoveFiles { files, .. } => files.len(),
        }
    }

    fn removes_folder(&self) -> bool {
        match self {
            Self::RemoveSet { .. } => true,
            Self::RemoveFiles { empties, .. } => *empties,
        }
    }
}

/// Resolve a matched set's difficulties to concrete .osu files on disk.
/// A file is deletable only when its name is a safe relative path, the file
/// exists, and (when the db records one) its md5 matches the content.
/// Returns `None` when nothing in the set can be deleted safely.
fn plan_map_removal(
    set: &BeatmapSet,
    dir: &Path,
    expr: &Expr,
    skips: &mut FileSkips,
) -> Option<Action> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for d in set.matched_difficulties(expr) {
        let Some(name) = d.osu_file.as_deref().filter(|s| clean::is_safe_rel_path(s)) else {
            skips.no_name += 1;
            continue;
        };
        let path = dir.join(name);
        if !seen.insert(path.clone()) {
            continue; // duplicate db entry for the same file
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            skips.missing += 1;
            continue;
        };
        if !meta.is_file() {
            skips.missing += 1;
            continue;
        }
        if !md5_matches(&path, d.md5.as_deref()) {
            skips.verify_failed += 1;
            continue;
        }
        files.push(FilePlan {
            path,
            size: meta.len(),
        });
    }
    if files.is_empty() {
        return None;
    }
    // Compare against .osu files counted on disk (not db entries), so a
    // folder only goes when no difficulty at all survives.
    let empties = clean::count_osu_files(dir) == files.len();
    let size = if empties {
        clean::dir_size(dir)
    } else {
        files.iter().map(|f| f.size).sum()
    };
    Some(Action::RemoveFiles {
        files,
        empties,
        size,
    })
}

/// True when the file content matches the db's md5 record; a db entry
/// without a hash falls back to name-only matching. Unreadable files
/// never match.
fn md5_matches(path: &Path, expect: Option<&str>) -> bool {
    let Some(expect) = expect.filter(|s| !s.is_empty()) else {
        return true;
    };
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let actual = encode_to_string(Md5::digest(data.as_slice()), AsciiCase::Lower);
    actual.eq_ignore_ascii_case(expect)
}

struct Target<'a> {
    set: &'a BeatmapSet,
    path: PathBuf,
    action: Action,
}

fn print_preview(
    targets: &[Target<'_>],
    existing: usize,
    missing: usize,
    unsafe_names: usize,
    file_skips: &FileSkips,
    target_mode: TargetMode,
    expr: &Expr,
    args: &Args,
) {
    println!();
    if targets.is_empty() {
        println!("没有匹配的谱面集。");
    } else {
        let total: u64 = targets.iter().map(|t| t.action.size()).sum();
        if target_mode == TargetMode::Set {
            println!(
                "匹配 {} / {} 个谱面集，合计 {}：",
                targets.len(),
                existing,
                human_size(total)
            );
        } else {
            let file_total: usize = targets.iter().map(|t| t.action.file_count()).sum();
            println!(
                "匹配 {} / {} 个谱面集，将删除 {file_total} 个难度文件，合计 {}：",
                targets.len(),
                existing,
                human_size(total)
            );
        }
        for (n, t) in targets.iter().take(args.sample).enumerate() {
            match target_mode {
                TargetMode::Set => println!(
                    "  {:>3}. {} | {} 个难度, {} 个匹配 | {}",
                    n + 1,
                    t.set.display_name(),
                    t.set.maps.len(),
                    t.set.matched_count(expr),
                    human_size(t.action.size())
                ),
                TargetMode::Map => {
                    let (count, note) = match &t.action {
                        Action::RemoveFiles { files, empties, .. } => (
                            files.len(),
                            if *empties {
                                "（整组移除）"
                            } else {
                                ""
                            },
                        ),
                        // Map mode never plans whole-set removals.
                        Action::RemoveSet { .. } => (0, ""),
                    };
                    println!(
                        "  {:>3}. {} | {} 个难度, 删 {count} 个{note} | {}",
                        n + 1,
                        t.set.display_name(),
                        t.set.maps.len(),
                        human_size(t.action.size())
                    );
                },
            }
        }
        if targets.len() > args.sample {
            println!(
                "  ... 其余 {} 个未显示（--sample 调整）",
                targets.len() - args.sample
            );
        }
        if missing > 0 {
            println!(
                "提示: 另有 {missing} 个匹配谱面集在磁盘上不存在，已跳过（数据库与 Songs \
                 不同步？）"
            );
        }
        if unsafe_names > 0 {
            eprintln!("警告: {unsafe_names} 个匹配谱面集的目录名异常，已跳过");
        }
        if file_skips.no_name > 0 {
            println!("提示: {} 个匹配难度缺少文件名，已跳过", file_skips.no_name);
        }
        if file_skips.missing > 0 {
            println!(
                "提示: {} 个匹配难度文件在磁盘上不存在，已跳过",
                file_skips.missing
            );
        }
        if file_skips.verify_failed > 0 {
            eprintln!(
                "警告: {} 个难度文件内容与数据库记录不符（已修改？无法读取？），已跳过",
                file_skips.verify_failed
            );
        }
        let note = match target_mode {
            TargetMode::Set => "将把以上目录移动到回收站。",
            TargetMode::Map => "将把以上难度文件移动到回收站；难度全部删除的谱面集目录会一并移除。",
        };
        println!("\n{note}");
    }
}

fn delete_targets(targets: &[Target<'_>], songs_dir: &Path) {
    println!();
    let mut files_ok = 0usize;
    let mut folders_ok = 0usize;
    let mut failed = 0usize;
    for (n, t) in targets.iter().enumerate() {
        match &t.action {
            Action::RemoveSet { .. } => {
                match trash::delete(&t.path) {
                    Ok(()) => {
                        folders_ok += 1;
                        // Nested folders (e.g. `<set>\mini`) may leave an empty
                        // parent behind; drop it when possible, keep Songs/ itself.
                        if let Some(parent) = t.path.parent() {
                            if parent != songs_dir {
                                let _ = std::fs::remove_dir(parent);
                            }
                        }
                    },
                    Err(e) => {
                        failed += 1;
                        eprintln!("失败 {}: {e}", t.path.display());
                    },
                }
            },
            Action::RemoveFiles { files, empties, .. } => {
                for f in files {
                    match trash::delete(&f.path) {
                        Ok(()) => files_ok += 1,
                        Err(e) => {
                            failed += 1;
                            eprintln!("失败 {}: {e}", f.path.display());
                        },
                    }
                }
                // Re-check the disk: only remove the folder when no .osu
                // difficulty survives (failed file deletions keep it alive).
                if *empties && clean::count_osu_files(&t.path) == 0 {
                    match trash::delete(&t.path) {
                        Ok(()) => folders_ok += 1,
                        Err(e) => {
                            failed += 1;
                            eprintln!("失败 {}: {e}", t.path.display());
                        },
                    }
                }
            },
        }
        if (n + 1) % 20 == 0 {
            println!("进度 {}/{}", n + 1, targets.len());
        }
    }
    println!(
        "已移动 {files_ok} 个难度文件、{folders_ok} 个谱面集目录到回收站（可在回收站中恢复）。"
    );
    if failed > 0 {
        eprintln!("{failed} 个移动失败（文件被占用？关闭 osu! 后重试）。");
    }
}

/// Interactive Y/n. Uses inquire on a TTY; falls back to a plain stdin read
/// when stdin is piped (scripts, tests).
fn confirm(prompt: &str) -> Result<bool> {
    if io::stdin().is_terminal() {
        match inquire::Confirm::new(prompt).with_default(false).prompt() {
            Ok(yes) => Ok(yes),
            Err(
                inquire::InquireError::OperationCanceled
                | inquire::InquireError::OperationInterrupted,
            ) => Ok(false),
            Err(e) => Err(anyhow::Error::new(e).context("读取确认输入失败")),
        }
    } else {
        print!("{prompt} [y/N] ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let t = line.trim();
        Ok(t.eq_ignore_ascii_case("y") || t.eq_ignore_ascii_case("yes"))
    }
}

/// Extra guard used when everything on disk would be deleted.
fn prompt_strict() -> Result<bool> {
    print!("请输入 yes 确认全部移动到回收站: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    // Precision loss here is irrelevant for a human-readable size.
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_verified_before_delete() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.osu");
        std::fs::write(&f, b"hello").unwrap();
        let md5 = encode_to_string(Md5::digest(b"hello"), AsciiCase::Lower);
        assert!(md5_matches(&f, Some(&md5)));
        assert!(md5_matches(&f, Some(&md5.to_uppercase())));
        assert!(md5_matches(&f, None)); // db without a hash → name-only match
        assert!(!md5_matches(&f, Some("00000000000000000000000000000000")));
        // stale hash after the file changed on disk → never delete
        std::fs::write(&f, b"changed").unwrap();
        assert!(!md5_matches(&f, Some(&md5)));
        assert!(!md5_matches(&dir.path().join("missing.osu"), Some(&md5)));
    }
}
