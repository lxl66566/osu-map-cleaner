//! Read-only Songs/ usage report: what the space is made of, size
//! distribution per gamemode and per star range, plus biggest offenders.
//!
//! Usage: cargo run --release --example analyze -- [osu_dir]

use std::{
    cmp::Reverse,
    collections::HashMap,
    env, fs,
    ops::AddAssign,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use osu_map_cleaner::{
    clean::{BeatmapSet, group_sets},
    db,
    expr::Mode,
};

const GIB: f64 = 1_073_741_824.0;
const MIB: f64 = 1_048_576.0;
/// Files at least this big are collected for the "biggest files" listing.
const BIG_FILE: u64 = 64 * 1024 * 1024;

// ---- classifications (compile-time checked, no magic strings) ----

/// Space成分 by file kind, decided by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Category {
    Beatmap,
    Audio,
    Video,
    Image,
    Hitsound,
    Archive,
    Other,
}

impl Category {
    fn from_ext(ext: &str) -> Self {
        match ext {
            "osu" => Self::Beatmap,
            "mp3" | "ogg" | "m4a" | "flac" | "opus" | "aac" | "wma" => Self::Audio,
            "mp4" | "flv" | "avi" | "mkv" | "webm" | "mov" | "wmv" | "m4v" | "mpg" | "mpeg" => {
                Self::Video
            },
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" => Self::Image,
            // wav in a set folder is almost always a hitsound sample, not music
            "wav" => Self::Hitsound,
            "zip" | "osz" | "rar" | "7z" => Self::Archive,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Beatmap => "osu",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Image => "image",
            Self::Hitsound => "hitsound",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }
}

/// Gamemode of a whole set folder; mixed when difficulties disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SetMode {
    Std,
    Taiko,
    Catch,
    Mania,
    Mixed,
}

impl SetMode {
    fn unified(mode: Mode) -> Self {
        match mode {
            Mode::Standard => Self::Std,
            Mode::Taiko => Self::Taiko,
            Mode::Catch => Self::Catch,
            Mode::Mania => Self::Mania,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Std => "std",
            Self::Taiko => "taiko",
            Self::Catch => "catch",
            Self::Mania => "mania",
            Self::Mixed => "mixed",
        }
    }
}

/// Star bucket of a set, by its hardest nomod difficulty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum StarBucket {
    Under1,
    From1To3,
    From3To4,
    From4To5,
    From5To6,
    Over6,
    Unknown,
}

impl StarBucket {
    fn from_star(star: Option<f64>) -> Self {
        let Some(s) = star else {
            return Self::Unknown;
        };
        if s < 1.0 {
            Self::Under1
        } else if s < 3.0 {
            Self::From1To3
        } else if s < 4.0 {
            Self::From3To4
        } else if s < 5.0 {
            Self::From4To5
        } else if s < 6.0 {
            Self::From5To6
        } else {
            Self::Over6
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Under1 => "<1",
            Self::From1To3 => "1-3",
            Self::From3To4 => "3-4",
            Self::From4To5 => "4-5",
            Self::From5To6 => "5-6",
            Self::Over6 => "6+",
            Self::Unknown => "?",
        }
    }
}

// ---- scan structures ----

#[derive(Default, Clone, Copy)]
struct Acc {
    size: u64,
    count: u64,
}

impl AddAssign for Acc {
    fn add_assign(&mut self, o: Self) {
        self.size += o.size;
        self.count += o.count;
    }
}

/// Per-worker scan results, merged after all workers join.
#[derive(Default)]
struct Scan {
    cat: HashMap<Category, Acc>,
    ext: HashMap<String, Acc>,
    /// One row per top-level Songs folder.
    sets: Vec<(String, u64, u64)>,
    /// Files of at least BIG_FILE bytes, path relative to Songs/.
    big: Vec<(u64, PathBuf, Category)>,
    errors: usize,
}

impl Scan {
    fn merge(&mut self, o: Self) {
        for (k, v) in o.cat {
            *self.cat.entry(k).or_default() += v;
        }
        for (k, v) in o.ext {
            *self.ext.entry(k).or_default() += v;
        }
        self.sets.extend(o.sets);
        self.big.extend(o.big);
        self.errors += o.errors;
    }
}

/// Recursively scan one folder tree into `scan`.
fn walk(dir: &Path, root: &Path, scan: &mut Scan, size: &mut u64, files: &mut u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        scan.errors += 1;
        return;
    };
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            scan.errors += 1;
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk(&entry.path(), root, scan, size, files);
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            scan.errors += 1;
            continue;
        };
        let s = meta.len();
        let ext = entry
            .path()
            .extension()
            .map_or_else(String::new, |e| e.to_string_lossy().to_ascii_lowercase());
        let cat = Category::from_ext(&ext);
        *scan.cat.entry(cat).or_default() += Acc { size: s, count: 1 };
        *scan.ext.entry(ext).or_default() += Acc { size: s, count: 1 };
        if s >= BIG_FILE {
            let path = entry
                .path()
                .strip_prefix(root)
                .map_or_else(|_| entry.path(), ToOwned::to_owned);
            scan.big.push((s, path, cat));
        }
        *size += s;
        *files += 1;
    }
}

/// Parallel scan of all top-level Songs folders. Returns the merged scan,
/// stats of loose files directly under Songs/, and directory-list errors.
fn scan_songs(songs: &Path) -> anyhow::Result<(Scan, Acc, usize)> {
    let mut top_dirs = Vec::new();
    let mut loose = Acc::default();
    let mut list_errors = 0usize;
    for entry in fs::read_dir(songs)
        .context("无法列出 Songs 目录")?
        .flatten()
    {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => top_dirs.push(entry.path()),
            Ok(_) => {
                if let Ok(m) = entry.metadata() {
                    loose += Acc {
                        size: m.len(),
                        count: 1,
                    };
                }
            },
            Err(_) => list_errors += 1,
        }
    }

    let total = top_dirs.len();
    let done = Arc::new(AtomicUsize::new(0));
    let workers = thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let mut scan = Scan::default();
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let dirs = &top_dirs;
            let done = &done;
            handles.push(scope.spawn(move || {
                let mut local = Scan::default();
                // round-robin so oversized folders spread across workers
                for dir in dirs.iter().skip(w).step_by(workers) {
                    let (mut size, mut files) = (0, 0);
                    walk(dir, songs, &mut local, &mut size, &mut files);
                    let name = dir
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    local.sets.push((name, size, files));
                    done.fetch_add(1, Ordering::Relaxed);
                }
                local
            }));
        }
        loop {
            let n = done.load(Ordering::Relaxed);
            if n >= total {
                break;
            }
            eprint!("\r扫描中 {n}/{total} 个谱面集目录…");
            thread::sleep(Duration::from_millis(500));
        }
        eprintln!();
        for h in handles {
            scan.merge(h.join().expect("扫描线程 panic"));
        }
    });
    let errors = list_errors + scan.errors;
    Ok((scan, loose, errors))
}

// ---- db join ----

/// db-side knowledge about one top-level Songs folder.
struct SetMeta {
    display: String,
    diffs: usize,
    /// None when difficulties of the folder disagree.
    mode: Option<Mode>,
    /// Negative when no difficulty carries a star rating.
    max_star: f64,
}

/// First path component of a db folder entry; newer osu! stores some sets in
/// nested folders (`<set>\mini`) sharing one top-level directory.
fn top_component(folder: &str) -> &str {
    folder.split(['/', '\\']).next().unwrap_or("")
}

fn set_metas(sets: &[BeatmapSet]) -> HashMap<String, SetMeta> {
    let mut metas: HashMap<String, SetMeta> = HashMap::new();
    for set in sets {
        // key lowercased: NTFS is case-insensitive, db/disk may differ in case
        let key = top_component(&set.folder).to_lowercase();
        let mut modes = set.maps.iter().map(|d| d.info.mode);
        let first = modes.next();
        let set_mode = if modes.all(|m| Some(m) == first) {
            first
        } else {
            None
        };
        let max_star = set
            .maps
            .iter()
            .filter_map(|d| d.info.star)
            .fold(f64::NEG_INFINITY, f64::max);
        let e = metas.entry(key).or_insert_with(|| SetMeta {
            display: set.display_name(),
            diffs: 0,
            mode: set_mode,
            max_star: f64::NEG_INFINITY,
        });
        e.diffs += set.maps.len();
        if e.mode != set_mode {
            e.mode = None;
        }
        e.max_star = e.max_star.max(max_star);
    }
    metas
}

/// One disk folder joined with its db metadata (None = not in db).
struct Joined {
    dir: String,
    size: u64,
    display: Option<String>,
    diffs: usize,
    mode: Option<Mode>,
    max_star: f64,
}

fn join_db(scan: &Scan, metas: &HashMap<String, SetMeta>) -> Vec<Joined> {
    scan.sets
        .iter()
        .map(|(dir, size, _files)| match metas.get(&dir.to_lowercase()) {
            Some(m) => Joined {
                display: Some(m.display.clone()),
                diffs: m.diffs,
                mode: m.mode,
                max_star: m.max_star,
                dir: dir.clone(),
                size: *size,
            },
            None => Joined {
                display: None,
                diffs: 0,
                mode: None,
                max_star: f64::NEG_INFINITY,
                dir: dir.clone(),
                size: *size,
            },
        })
        .collect()
}

#[derive(Default)]
struct Agg {
    sets: usize,
    diffs: usize,
    size: u64,
}

impl Agg {
    fn add(&mut self, diffs: usize, size: u64) {
        self.sets += 1;
        self.diffs += diffs;
        self.size += size;
    }
}

// ---- formatting helpers ----

#[allow(clippy::cast_precision_loss)]
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB)
}

#[allow(clippy::cast_precision_loss)]
fn mib(bytes: u64) -> String {
    let v = bytes as f64 / MIB;
    format!("{v:.0} MiB")
}

#[allow(clippy::cast_precision_loss)]
fn pct(part: u64, total: u64) -> String {
    if total == 0 {
        return "0.0%".into();
    }
    format!("{:.1}%", part as f64 / total as f64 * 100.0)
}

// ---- report sections ----

fn report_overview(joined: &[Joined], total_files: u64, loose: Acc) {
    let total: u64 = joined.iter().map(|j| j.size).sum();
    #[allow(clippy::cast_precision_loss)]
    let avg = total as f64 / joined.len().max(1) as f64 / MIB;
    println!("== 总览 ==");
    println!(
        "磁盘: {} 个谱面集目录, {total_files} 个文件, 共 {} (平均 {avg:.1} MiB/集)",
        joined.len(),
        gib(total)
    );
    if loose.count > 0 {
        println!(
            "Songs 根目录散落文件: {} 个, {}",
            loose.count,
            gib(loose.size)
        );
    }
}

fn report_categories(cat: &HashMap<Category, Acc>, total: u64) {
    println!("\n== 空间成分 (按扩展名分类) ==");
    let mut rows: Vec<_> = cat.iter().collect();
    rows.sort_unstable_by_key(|(_, a)| Reverse(a.size));
    for (c, a) in rows {
        println!(
            "  {:<10} {:>12}  {:>6}  {} 个文件",
            c.label(),
            gib(a.size),
            pct(a.size, total),
            a.count
        );
    }
}

fn report_exts(ext: &HashMap<String, Acc>, total: u64) {
    println!("\n== 扩展名 top 15 ==");
    let mut rows: Vec<_> = ext.iter().collect();
    rows.sort_unstable_by_key(|(_, a)| Reverse(a.size));
    for (e, a) in rows.into_iter().take(15) {
        let name = if e.is_empty() {
            "(无扩展名)"
        } else {
            e.as_str()
        };
        println!(
            "  {:<10} {:>12}  {:>6}  {} 个文件",
            name,
            gib(a.size),
            pct(a.size, total),
            a.count
        );
    }
}

fn report_modes(joined: &[Joined], total: u64, unindexed: &Agg) {
    println!("\n== 模式分布 (大小按整个谱面集目录) ==");
    println!(
        "  {:<7} {:>8} {:>8} {:>12}  {:>6}",
        "mode", "集数", "难度数", "大小", "占比"
    );
    let mut by_mode: HashMap<SetMode, Agg> = HashMap::new();
    for j in joined.iter().filter(|j| j.display.is_some()) {
        let m = j.mode.map_or(SetMode::Mixed, SetMode::unified);
        by_mode.entry(m).or_default().add(j.diffs, j.size);
    }
    let mut rows: Vec<_> = by_mode.into_iter().collect();
    rows.sort_unstable_by_key(|(_, a)| Reverse(a.size));
    for (m, a) in rows {
        println!(
            "  {:<7} {:>8} {:>8} {:>12}  {:>6}",
            m.label(),
            a.sets,
            a.diffs,
            gib(a.size),
            pct(a.size, total)
        );
    }
    if unindexed.sets > 0 {
        println!(
            "  {:<7} {:>8} {:>8} {:>12}  {:>6}",
            "(未索引)",
            unindexed.sets,
            "-",
            gib(unindexed.size),
            pct(unindexed.size, total)
        );
    }
}

/// Star availability per mode, explaining the "?" bucket: a difficulty with
/// no usable rating in the db (osu! computes stars lazily) cannot be bucketed.
fn report_star_availability(sets: &[BeatmapSet]) {
    // star availability per mode: [mode, with stars, without]
    let mut avail: [(Mode, usize, usize); 4] = [
        (Mode::Standard, 0, 0),
        (Mode::Taiko, 0, 0),
        (Mode::Catch, 0, 0),
        (Mode::Mania, 0, 0),
    ];
    let slot = |m: Mode| match m {
        Mode::Standard => 0,
        Mode::Taiko => 1,
        Mode::Catch => 2,
        Mode::Mania => 3,
    };
    for set in sets {
        for d in &set.maps {
            let i = slot(d.info.mode);
            if d.info.star.is_some() {
                avail[i].1 += 1;
            } else {
                avail[i].2 += 1;
            }
        }
    }
    println!("\n== 星数可用性 (db 中有 nomod 星数的难度比例) ==");
    for (m, with, without) in avail {
        let total = with + without;
        #[allow(clippy::cast_precision_loss)]
        let rate = with as f64 / total.max(1) as f64 * 100.0;
        println!(
            "  {:<7} {with:>8} / {total:<8} ({rate:.1}%)",
            SetMode::unified(m).label()
        );
    }
}

fn report_stars(joined: &[Joined], total: u64) {
    println!("\n== 星数分布 (按集内最高难度 nomod 星数分桶) ==");
    println!(
        "  {:<5} {:>8} {:>8} {:>12}  {:>6}",
        "星数", "集数", "难度数", "大小", "占比"
    );
    let mut by_star: HashMap<StarBucket, Agg> = HashMap::new();
    for j in joined.iter().filter(|j| j.display.is_some()) {
        let star = j.max_star.is_finite().then_some(j.max_star);
        by_star
            .entry(StarBucket::from_star(star))
            .or_default()
            .add(j.diffs, j.size);
    }
    let mut rows: Vec<_> = by_star.into_iter().collect();
    rows.sort_unstable_by_key(|(b, _)| *b);
    for (b, a) in rows {
        println!(
            "  {:<5} {:>8} {:>8} {:>12}  {:>6}",
            b.label(),
            a.sets,
            a.diffs,
            gib(a.size),
            pct(a.size, total)
        );
    }
}

fn report_tops(joined: &mut [Joined], big: &mut [(u64, PathBuf, Category)]) {
    joined.sort_unstable_by_key(|j| Reverse(j.size));
    println!("\n== 最大谱面集 top 20 ==");
    for (i, j) in joined.iter().take(20).enumerate() {
        let star = if j.max_star.is_finite() {
            format!("{:.1}★", j.max_star)
        } else {
            "?".into()
        };
        println!(
            "  {:>2}. {:>12}  {:<5} {:>6}  {}",
            i + 1,
            gib(j.size),
            j.mode.map_or("?", |m| SetMode::unified(m).label()),
            star,
            j.display.clone().unwrap_or_else(|| j.dir.clone())
        );
    }
    big.sort_unstable_by_key(|(size, ..)| Reverse(*size));
    println!("\n== 最大单文件 top 15 (仅记录 ≥64 MiB) ==");
    for (i, (size, path, cat)) in big.iter().enumerate().take(15) {
        println!(
            "  {:>2}. {:>10}  {:<8} {}",
            i + 1,
            mib(*size),
            cat.label(),
            path.display()
        );
    }
}

// ---- main ----

fn run(osu_dir: &Path) -> anyhow::Result<()> {
    let db_path = osu_dir.join("osu!.db");
    let songs = osu_dir.join("Songs");
    if !db_path.is_file() {
        bail!("未找到 {}", db_path.display());
    }
    if !songs.is_dir() {
        bail!("未找到 {}", songs.display());
    }

    let bytes = fs::read(&db_path).with_context(|| format!("读取 {} 失败", db_path.display()))?;
    let parsed = db::parse(&bytes).with_context(|| format!("解析 {} 失败", db_path.display()))?;
    let (sets, skipped) = group_sets(&parsed.beatmaps);
    let metas = set_metas(&sets);
    println!("osu! 目录 : {}", osu_dir.display());
    println!(
        "数据库   : 版本 {}, {} 张谱面, {} 个谱面集{}",
        parsed.version,
        parsed.beatmaps.len(),
        sets.len(),
        if skipped > 0 {
            format!(", {skipped} 张无目录名已跳过")
        } else {
            String::new()
        }
    );

    let (mut scan, loose, errors) = scan_songs(&songs)?;
    let mut joined = join_db(&scan, &metas);
    let total_files: u64 = scan.sets.iter().map(|(_, _, f)| f).sum();
    let total: u64 = joined.iter().map(|j| j.size).sum();

    report_overview(&joined, total_files, loose);
    report_categories(&scan.cat, total);
    report_exts(&scan.ext, total);

    let unindexed: Agg =
        joined
            .iter()
            .filter(|j| j.display.is_none())
            .fold(Agg::default(), |mut a, j| {
                a.add(0, j.size);
                a
            });
    report_modes(&joined, total, &unindexed);
    report_star_availability(&sets);
    report_stars(&joined, total);
    report_tops(&mut joined, &mut scan.big);

    let db_missing = metas.len()
        - joined
            .iter()
            .filter(|j| metas.contains_key(&j.dir.to_lowercase()))
            .count();
    println!("\n== 一致性 ==");
    println!(
        "db 有而磁盘没有: {db_missing} 集; 磁盘有而 db 没有: {} 集 ({})",
        unindexed.sets,
        gib(unindexed.size)
    );
    if errors > 0 {
        println!("读取失败条目: {errors} (被占用/无权限, 其大小未计入)");
    }
    Ok(())
}

fn main() -> ExitCode {
    let start = Instant::now();
    let dir = env::args()
        .nth(1)
        .unwrap_or_else(|| "C:/game/osu".to_owned());
    let code = match run(Path::new(&dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e:#}");
            ExitCode::FAILURE
        },
    };
    eprintln!("耗时 {:.1}s", start.elapsed().as_secs_f32());
    code
}
