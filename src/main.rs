//! CLI: filter osu! beatmap sets by expression and move them to the recycle bin.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf, absolute},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use osu_map_cleaner::{
    clean::{self, BeatmapSet, MatchMode},
    expr::Expr,
};

#[derive(Parser)]
#[command(
    version,
    about = "按过滤表达式批量清理 osu! 谱面集（移动到回收站，可恢复）",
    after_help = "示例:\n  osu-map-cleaner \"key=7 star<3\"\n  osu-map-cleaner key=7 star<3 -d \
                  C:\\game\\osu --dry-run\n  osu-map-cleaner \"star<2 length<60 mode=mania\" \
                  --match all"
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

    /// 仅预览匹配结果，不删除
    #[arg(long)]
    dry_run: bool,
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
    println!("表达式   : {expr}  ({})", match_mode.label());

    let db_bytes = std::fs::read(&db_path).with_context(|| {
        format!(
            "读取 {} 失败（osu! 正在运行？权限不足？）",
            db_path.display()
        )
    })?;
    let db = osu_map_cleaner::db::parse(&db_bytes)
        .with_context(|| format!("解析 {} 失败（db 版本不受支持？）", db_path.display()))?;
    let (sets, skipped) = clean::group_sets(&db.beatmaps);
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

    // Resolve folders for all sets (not only matched ones) so the
    // "matched everything?" guard counts what actually exists on disk.
    let mut targets = Vec::new();
    let mut existing = 0usize;
    let mut missing = 0usize;
    let mut unsafe_names = 0usize;
    let songs_canon = songs_dir.canonicalize().context("无法访问 Songs 目录")?;
    for set in &sets {
        let matched = set.matches(&expr, match_mode);
        if !clean::is_safe_folder_path(&set.folder) {
            unsafe_names += usize::from(matched);
            continue;
        }
        // is_safe_folder_path rejects `..` and absolute prefixes, so a
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
        if matched {
            let size = clean::dir_size(&path);
            targets.push(Target { set, path, size });
        }
    }

    print_preview(&targets, existing, missing, unsafe_names, &expr, &args);

    if targets.is_empty() {
        return Ok(());
    }
    if args.dry_run {
        println!("--dry-run: 仅预览，未删除任何内容。");
        return Ok(());
    }

    // Deleting every set on disk is almost certainly a mistake; require the
    // full word instead of a single keypress.
    let confirmed = if targets.len() == existing {
        println!("\n*** 警告: 表达式命中了磁盘上全部 {existing} 个谱面集！***");
        prompt_strict()?
    } else {
        confirm("确认将以上谱面集移动到回收站?")?
    };
    if !confirmed {
        println!("已取消，未删除任何内容。");
        return Ok(());
    }
    delete_targets(&targets, &songs_dir);
    Ok(())
}

struct Target<'a> {
    set: &'a BeatmapSet,
    path: PathBuf,
    size: u64,
}

fn print_preview(
    targets: &[Target<'_>],
    existing: usize,
    missing: usize,
    unsafe_names: usize,
    expr: &Expr,
    args: &Args,
) {
    println!();
    if targets.is_empty() {
        println!("没有匹配的谱面集。");
    } else {
        let total: u64 = targets.iter().map(|t| t.size).sum();
        println!(
            "匹配 {} / {} 个谱面集，合计 {}：",
            targets.len(),
            existing,
            human_size(total)
        );
        for (n, t) in targets.iter().take(args.sample).enumerate() {
            println!(
                "  {:>3}. {} | {} 个难度, {} 个匹配 | {}",
                n + 1,
                t.set.display_name(),
                t.set.maps.len(),
                t.set.matched_count(expr),
                human_size(t.size)
            );
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
        println!("\n将把以上目录移动到回收站。");
    }
}

fn delete_targets(targets: &[Target<'_>], songs_dir: &Path) {
    println!();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for (n, t) in targets.iter().enumerate() {
        match trash::delete(&t.path) {
            Ok(()) => {
                ok += 1;
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
        if (n + 1) % 20 == 0 {
            println!("进度 {}/{}", n + 1, targets.len());
        }
    }
    println!("已移动 {ok} 个谱面集到回收站（可在回收站中恢复）。");
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
