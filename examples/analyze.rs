//! Read-only Songs/ usage report: what the space is made of, and its
//! distribution across gamemode (mania per key count) and star range.
//!
//! Unlike the cleaner itself this is analysis-only; it never deletes.
//! Sizing semantics:
//! - Whole-folder reports (composition, tops) use raw directory sizes.
//! - Mode/star reports split each folder across its difficulties: audio
//!   files are divided among the difficulties referencing them, everything
//!   else (backgrounds, videos, ...) equally among all difficulties. Mixed
//!   std+mania sets therefore land in both modes instead of a "mixed" bin.
//! - Stars come from the db's nomod value; missing ones are backfilled
//!   locally with rosu-pp (lazer algorithm), matching `stars.rs` safety.
//!
//! Usage: cargo run --release --example analyze -- [osu_dir]

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    env, fs,
    ops::AddAssign,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Instant,
};

use anyhow::{Context, bail};
use osu_map_cleaner::{clean::is_safe_rel_path, db};
use rayon::prelude::*;
use rosu_pp::model::mode::GameMode;

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

// ---- per-difficulty model ----

/// One difficulty: everything the mode/star distribution needs.
struct Diff {
    mode: u8,
    circle_size: f32,
    star: Option<f64>,
    /// Audio file name (lowercased) as recorded in the db.
    audio: Option<String>,
    /// .osu file name for star backfill.
    file: Option<String>,
}

/// All db entries of one Songs folder.
struct Set {
    folder: String,
    diffs: Vec<Diff>,
}

/// Bucket key: 0/1/2 = std/taiko/catch, 100+k = mania with k keys.
/// Star: -1 unknown, 0 = <1★, 1..=9 = k..k+1★, 10 = 10+★.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Bucket {
    mode_key: u16,
    star: i8,
}

fn mode_key_of(mode: u8, cs: f32) -> u16 {
    match mode {
        0 => 0,
        1 => 1,
        2 => 2,
        // mania: rounded CS is the key count, legal range 1..=18
        3 => {
            #[allow(clippy::cast_possible_truncation)] // clamped right after
            let k = f64::from(cs).round() as i16;
            (100 + u16::try_from(k.clamp(1, 18)).unwrap_or(18)).min(118)
        },
        _ => 999,
    }
}

fn mode_label(mk: u16) -> String {
    match mk {
        0 => "std".to_owned(),
        1 => "taiko".to_owned(),
        2 => "catch".to_owned(),
        k if (100..118).contains(&k) => format!("mania {}K", k - 100),
        k if k == 118 => "mania 18K+".to_owned(),
        _ => format!("mode{mk}"),
    }
}

fn star_bucket(star: Option<f64>) -> i8 {
    let Some(s) = star.filter(|s| s.is_finite()) else {
        return -1;
    };
    if s < 1.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation)] // clamped right after
    let k = s.floor() as i8;
    k.clamp(1, 10)
}

fn star_label(s: i8) -> String {
    match s {
        -1 => "未知".to_owned(),
        0 => "<1★".to_owned(),
        10 => "10+★".to_owned(),
        k => format!("{k}-{k2}★", k2 = k + 1),
    }
}

/// Same rules as `clean::nomod_star`.
fn nomod_star(b: &db::Beatmap) -> Option<f64> {
    let ratings = &b.ratings[usize::from(b.mode)];
    ratings
        .iter()
        .find(|&&(mods, _)| mods == 0)
        .map(|&(_, sr)| sr)
        .filter(|&sr| sr > 0.0)
        .or_else(|| ratings.iter().map(|&(_, sr)| sr).find(|&sr| sr > 0.0))
}

// ---- star backfill ----

#[derive(Default)]
struct FillStats {
    computed: usize,
    skipped: usize,
    missing: usize,
    failed: usize,
}

impl AddAssign for FillStats {
    fn add_assign(&mut self, o: Self) {
        self.computed += o.computed;
        self.skipped += o.skipped;
        self.missing += o.missing;
        self.failed += o.failed;
    }
}

/// Fill in stars for difficulties without a db value. Same safety rules as
/// `stars.rs`: unsafe names, missing files and uncomputable maps stay
/// unknown; only positive stars are accepted.
fn fill_stars(sets: &mut [Set], songs: &Path) -> FillStats {
    const PROGRESS_EVERY: usize = 4_000;
    let total = sets.len();
    let done = Arc::new(AtomicUsize::new(0));
    let stats = Arc::new(AtomicUsize::new(0)); // computed, for progress only
    let out = sets
        .par_iter_mut()
        .map(|s| {
            let mut st = FillStats::default();
            let folder_ok = is_safe_rel_path(&s.folder);
            for d in &mut s.diffs {
                if d.star.is_some() {
                    continue;
                }
                let Some(name) = d.file.as_deref().filter(|n| is_safe_rel_path(n)) else {
                    st.skipped += 1;
                    continue;
                };
                if !folder_ok {
                    st.skipped += 1;
                    continue;
                }
                let path = songs.join(&s.folder).join(name);
                if !path.is_file() {
                    st.missing += 1;
                    continue;
                }
                match compute_star(&path, d.mode) {
                    Some(sr) => {
                        d.star = Some(sr);
                        st.computed += 1;
                    },
                    None => st.failed += 1,
                }
            }
            if st.computed > 0 {
                stats.fetch_add(st.computed, Ordering::Relaxed);
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % PROGRESS_EVERY == 0 {
                eprintln!("星数补算 {n}/{total} 集（已算 {} 个）", stats.load(Ordering::Relaxed));
            }
            st
        })
        .reduce(FillStats::default, |mut a, b| {
            a += b;
            a
        });
    eprintln!();
    out
}

/// Same rules as `stars.rs`: empty maps, mode mismatches and failures
/// yield `None`; only positive stars are returned.
fn compute_star(path: &Path, mode: u8) -> Option<f64> {
    let map = rosu_pp::Beatmap::from_path(path).ok()?;
    if map.hit_objects.is_empty() {
        return None;
    }
    let target = match mode {
        0 => GameMode::Osu,
        1 => GameMode::Taiko,
        2 => GameMode::Catch,
        _ => GameMode::Mania,
    };
    if target != map.mode && map.mode != GameMode::Osu {
        return None;
    }
    let diff = rosu_pp::Difficulty::new();
    let stars = match target {
        GameMode::Osu => diff.checked_calculate_for_mode::<rosu_pp::osu::Osu>(&map).ok()?.stars,
        GameMode::Taiko => diff
            .checked_calculate_for_mode::<rosu_pp::taiko::Taiko>(&map)
            .ok()?
            .stars,
        GameMode::Catch => diff
            .checked_calculate_for_mode::<rosu_pp::catch::Catch>(&map)
            .ok()?
            .stars,
        GameMode::Mania => diff
            .checked_calculate_for_mode::<rosu_pp::mania::Mania>(&map)
            .ok()?
            .stars,
    };
    (stars > 0.0).then_some(stars)
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

/// One scanned top-level Songs folder.
struct DirInfo {
    name: String,
    size: u64,
    files: u64,
    /// (lowercased file name, size) of every file in the tree.
    list: Vec<(String, u64)>,
}

/// Per-worker scan results, merged after all workers join.
#[derive(Default)]
struct Scan {
    cat: HashMap<Category, Acc>,
    ext: HashMap<String, Acc>,
    dirs: Vec<DirInfo>,
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
        self.dirs.extend(o.dirs);
        self.big.extend(o.big);
        self.errors += o.errors;
    }
}

/// Recursively scan one folder tree into `scan`.
fn walk(dir: &Path, root: &Path, scan: &mut Scan, size: &mut u64, files: &mut u64, list: &mut Vec<(String, u64)>) {
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
            walk(&entry.path(), root, scan, size, files, list);
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            scan.errors += 1;
            continue;
        };
        let s = meta.len();
        let name = entry.file_name().to_string_lossy().into_owned();
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
        list.push((name.to_lowercase(), s));
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
                    let mut list = Vec::new();
                    walk(dir, songs, &mut local, &mut size, &mut files, &mut list);
                    let name = dir
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    local.dirs.push(DirInfo {
                        name,
                        size,
                        files,
                        list,
                    });
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if n % 2_000 == 0 {
                        eprint!("\r扫描中 {n}/{total} 个谱面集目录…");
                    }
                }
                local
            }));
        }
        for h in handles {
            scan.merge(h.join().expect("扫描线程 panic"));
        }
    });
    eprintln!();
    let errors = list_errors + scan.errors;
    Ok((scan, loose, errors))
}

// ---- db join & size attribution ----

/// db-side knowledge about one top-level Songs folder. Difficulties of
/// nested db folders (`<set>\mini`) sharing the top directory are merged
/// here so the whole disk folder is attributable.
struct SetMeta {
    diffs: Vec<(Bucket, Option<String>)>, // bucket + audio name (lowercased)
    /// Negative when no difficulty carries a star rating.
    max_star: f64,
    /// Set-level mode for the tops listing: unified label or "mixed".
    unified_mode: Option<u16>,
}

/// First path component of a db folder entry; newer osu! stores some sets in
/// nested folders (`<set>\mini`) sharing one top-level directory.
fn top_component(folder: &str) -> &str {
    folder.split(['/', '\\']).next().unwrap_or("")
}

fn build_metas(sets: &[Set]) -> HashMap<String, SetMeta> {
    let mut metas: HashMap<String, SetMeta> = HashMap::new();
    for set in sets {
        // key lowercased: NTFS is case-insensitive, db/disk may differ in case
        let key = top_component(&set.folder).to_lowercase();
        let buckets: Vec<(Bucket, Option<String>)> = set
            .diffs
            .iter()
            .map(|d| {
                (
                    Bucket {
                        mode_key: mode_key_of(d.mode, d.circle_size),
                        star: star_bucket(d.star),
                    },
                    d.audio.clone(),
                )
            })
            .collect();
        let max_star = set
            .diffs
            .iter()
            .filter_map(|d| d.star)
            .fold(f64::NEG_INFINITY, f64::max);
        let unified = {
            let mks: HashSet<u16> = buckets.iter().map(|(b, _)| b.mode_key).collect();
            if mks.len() == 1 {
                mks.into_iter().next()
            } else {
                None
            }
        };
        let e = metas.entry(key).or_insert_with(|| SetMeta {
            diffs: Vec::new(),
            max_star: f64::NEG_INFINITY,
            unified_mode: unified,
        });
        e.diffs.extend(buckets);
        e.max_star = e.max_star.max(max_star);
        if e.unified_mode != unified {
            e.unified_mode = None;
        }
    }
    metas
}

/// Mode/star aggregation across all attributable folders.
#[derive(Default)]
struct Dist {
    buckets: HashMap<Bucket, (u64, u32)>, // size, difficulty count
    /// Folders containing at least one difficulty of each mode.
    mode_dirs: HashMap<u16, HashSet<String>>,
    attributed: u64,
}

/// Split one folder's size across its difficulties and accumulate into
/// `dist`. Audio bytes go to the difficulties referencing that audio
/// (divided among them); everything else is split equally.
fn attribute(dir: &DirInfo, meta: &SetMeta, dist: &mut Dist) {
    let n = meta.diffs.len();
    if n == 0 {
        return;
    }
    let mut refs: HashMap<&str, u32> = HashMap::new();
    for (b, audio) in &meta.diffs {
        dist.mode_dirs
            .entry(b.mode_key)
            .or_default()
            .insert(dir.name.clone());
        if let Some(a) = audio.as_deref() {
            *refs.entry(a).or_default() += 1;
        }
    }
    let mut audio_bytes: HashMap<&str, u64> = HashMap::new();
    for (name, size) in &dir.list {
        if refs.contains_key(name.as_str()) {
            *audio_bytes.entry(name.as_str()).or_default() += size;
        }
    }
    let referenced: u64 = audio_bytes.values().sum();
    let shared = dir.size.saturating_sub(referenced);
    #[allow(clippy::cast_sign_loss)] // n > 0 checked above
    let shared_per = shared / n as u64;

    for (b, audio) in &meta.diffs {
        let own = audio
            .as_deref()
            .and_then(|a| audio_bytes.get(a).copied())
            .unwrap_or(0)
            / u64::from(refs.get(audio.as_deref().unwrap_or("")).copied().unwrap_or(1).max(1));
        let e = dist.buckets.entry(*b).or_default();
        e.0 += own + shared_per;
        e.1 += 1;
    }
    dist.attributed += dir.size;
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

fn report_overview(scan: &Scan, total: u64, loose: Acc, fill: &FillStats, unknown_after: usize) {
    let dirs = scan.dirs.len();
    let files: u64 = scan.dirs.iter().map(|d| d.files).sum();
    #[allow(clippy::cast_precision_loss)]
    let avg = total as f64 / dirs.max(1) as f64 / MIB;
    println!("== 总览 ==");
    println!(
        "磁盘: {dirs} 个谱面集目录, {files} 个文件, 共 {} (平均 {avg:.1} MiB/集)",
        gib(total)
    );
    if loose.count > 0 {
        println!(
            "Songs 根目录散落文件: {} 个, {}",
            loose.count,
            gib(loose.size)
        );
    }
    println!(
        "星数补算 (rosu-pp): 成功 {} | 文件缺失/无名 {} | 失败 {} | 补算后仍未知 {unknown_after}",
        fill.computed,
        fill.skipped + fill.missing,
        fill.failed
    );
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

fn report_mode(dist: &Dist, total: u64, unindexed: (usize, u64)) {
    println!("\n== 模式分布 (目录大小按难度分摊; mania 精确到 key) ==");
    println!("  {:<12} {:>8} {:>8} {:>12}  {:>6}", "mode", "集数*", "难度数", "大小", "占比");
    let mut mode_keys: Vec<u16> = dist.buckets.keys().map(|b| b.mode_key).collect();
    mode_keys.sort_unstable();
    mode_keys.dedup();
    mode_keys.sort_by_key(|&k| (k >= 100, k)); // std/taiko/catch first, mania by keys
    for mk in mode_keys {
        let (size, diffs) = dist
            .buckets
            .iter()
            .filter(|(b, _)| b.mode_key == mk)
            .fold((0u64, 0u32), |(s, n), (_, (sz, dn))| (s + sz, n + dn));
        let sets = dist.mode_dirs.get(&mk).map_or(0, HashSet::len);
        println!(
            "  {:<12} {:>8} {:>8} {:>12}  {:>6}",
            mode_label(mk),
            sets,
            diffs,
            gib(size),
            pct(size, total)
        );
    }
    if unindexed.0 > 0 {
        println!(
            "  {:<12} {:>8} {:>8} {:>12}  {:>6}",
            "(未索引)",
            unindexed.0,
            "-",
            gib(unindexed.1),
            pct(unindexed.1, total)
        );
    }
    println!("* 含该模式难度的目录数；混合目录在多个模式重复计数；难度数仅含磁盘存在的目录");
}

fn report_mode_star(dist: &Dist, total: u64) {
    println!("\n== 模式 × 星级分布 (难度级分摊, 含补算星级) ==");
    let mut mode_keys: Vec<u16> = dist.buckets.keys().map(|b| b.mode_key).collect();
    mode_keys.sort_unstable();
    mode_keys.dedup();
    mode_keys.sort_by_key(|&k| (k >= 100, k));
    for mk in mode_keys {
        let subtotal: u64 = dist
            .buckets
            .iter()
            .filter(|(b, _)| b.mode_key == mk)
            .map(|(_, (s, _))| s)
            .sum();
        let label = mode_label(mk);
        println!("  {label}  小计 {} ({})", gib(subtotal), pct(subtotal, total));
        let mut stars: Vec<i8> = dist
            .buckets
            .iter()
            .filter(|(b, _)| b.mode_key == mk)
            .map(|(b, _)| b.star)
            .collect();
        stars.sort_unstable();
        stars.dedup();
        for s in stars {
            let Some(&(size, diffs)) = dist.buckets.get(&Bucket {
                mode_key: mk,
                star: s,
            }) else {
                continue;
            };
            println!(
                "    {:<8} {:>12}  {:>6}  {diffs} 难度",
                star_label(s),
                gib(size),
                pct(size, total)
            );
        }
    }
}

/// Star rows: -1 unknown, 0..=6 = exact bucket, 7 = 7+ (matrix collapse).
fn matrix_row(star: i8) -> i8 {
    if star < 0 { -1 } else { star.min(7) }
}

fn matrix_row_label(r: i8) -> String {
    match r {
        -1 => "未知".to_owned(),
        0 => "<1★".to_owned(),
        7 => "7+★".to_owned(),
        k => format!("{k}-{k2}★", k2 = k + 1),
    }
}

fn matrix_col_label(mk: u16) -> String {
    match mk {
        0 => "std".to_owned(),
        1 => "taiko".to_owned(),
        2 => "catch".to_owned(),
        118 => "18K+".to_owned(),
        k if k >= 100 => format!("{}K", k - 100),
        _ => format!("m{mk}"),
    }
}

/// Compact star × mode size matrix (GiB), 7+ collapsed into one row.
fn report_matrix(dist: &Dist) {
    let mut mks: Vec<u16> = dist.buckets.keys().map(|b| b.mode_key).collect();
    mks.sort_unstable();
    mks.dedup();
    mks.sort_by_key(|&k| (k >= 100, k));

    let mut cells: HashMap<(i8, u16), u64> = HashMap::new();
    let mut row_totals: HashMap<i8, u64> = HashMap::new();
    let mut col_totals: HashMap<u16, u64> = HashMap::new();
    for (b, (size, _)) in &dist.buckets {
        let r = matrix_row(b.star);
        *cells.entry((r, b.mode_key)).or_default() += size;
        *row_totals.entry(r).or_default() += size;
        *col_totals.entry(b.mode_key).or_default() += size;
    }
    let rows = [-1i8, 0, 1, 2, 3, 4, 5, 6, 7];

    println!("\n== 星级 × 模式矩阵 (大小, GiB) ==");
    print!("{:<7}", "");
    for mk in &mks {
        print!("{:>7}", matrix_col_label(*mk));
    }
    println!("{:>7}", "合计");
    for r in rows {
        print!("{:<7}", matrix_row_label(r));
        for mk in &mks {
            let size = cells.get(&(r, *mk)).copied().unwrap_or(0);
            print!("{:>7}", cell(size));
        }
        println!("{:>7}", cell(row_totals.get(&r).copied().unwrap_or(0)));
    }
    print!("{:<7}", "合计");
    let grand: u64 = row_totals.values().sum();
    for mk in &mks {
        print!("{:>7}", cell(col_totals.get(mk).copied().unwrap_or(0)));
    }
    println!("{:>7}", cell(grand));
}

/// GiB with one decimal; "-" for empty cells.
#[allow(clippy::cast_precision_loss)]
fn cell(size: u64) -> String {
    if size == 0 {
        "-".to_owned()
    } else {
        format!("{:.1}", size as f64 / GIB)
    }
}

fn report_tops(dirs: &mut [DirInfo], metas: &HashMap<String, SetMeta>, big: &mut [(u64, PathBuf, Category)]) {
    dirs.sort_unstable_by_key(|d| Reverse(d.size));
    println!("\n== 最大谱面集 top 20 ==");
    for (i, d) in dirs.iter().take(20).enumerate() {
        let (mode, star) = match metas.get(&d.name.to_lowercase()) {
            Some(m) => (
                m.unified_mode
                    .map_or_else(|| "mixed".to_owned(), mode_label),
                if m.max_star.is_finite() {
                    format!("{:.1}★", m.max_star)
                } else {
                    "?".into()
                },
            ),
            None => ("(未索引)".to_owned(), "?".into()),
        };
        println!(
            "  {:>2}. {:>12}  {:<11} {:>6}  {}",
            i + 1,
            gib(d.size),
            mode,
            star,
            d.name
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
    println!("osu! 目录 : {}", osu_dir.display());
    println!(
        "数据库   : 版本 {}, {} 张谱面",
        parsed.version,
        parsed.beatmaps.len()
    );

    // group db entries by Songs folder (full folder name)
    let mut sets: Vec<Set> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for b in &parsed.beatmaps {
        let Some(folder) = b.folder_name.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        let d = Diff {
            mode: b.mode,
            circle_size: b.circle_size,
            star: nomod_star(b),
            audio: b.audio.as_deref().map(str::to_lowercase),
            file: b.file_name.clone(),
        };
        match index.get(folder) {
            Some(&i) => sets[i].diffs.push(d),
            None => {
                index.insert(folder, sets.len());
                sets.push(Set {
                    folder: folder.to_owned(),
                    diffs: vec![d],
                });
            },
        }
    }
    let total_diffs: usize = sets.iter().map(|s| s.diffs.len()).sum();
    println!("按目录分组: {} 个谱面集, {total_diffs} 个难度", sets.len());

    let t = Instant::now();
    let fill = fill_stars(&mut sets, &songs);
    let unknown_after = sets
        .iter()
        .flat_map(|s| &s.diffs)
        .filter(|d| d.star.is_none())
        .count();
    println!("星数补算完成 ({:.1}s)", t.elapsed().as_secs_f32());
    let metas = build_metas(&sets);
    drop(sets);

    let (mut scan, loose, errors) = scan_songs(&songs)?;
    let dirs_total: u64 = scan.dirs.iter().map(|d| d.size).sum();
    let total = dirs_total + loose.size;

    // attribution across all folders
    let mut dist = Dist::default();
    let mut unindexed = (0usize, 0u64);
    let mut matched_metas = 0usize;
    for dir in &scan.dirs {
        match metas.get(&dir.name.to_lowercase()) {
            Some(meta) => {
                attribute(dir, meta, &mut dist);
                matched_metas += 1;
            },
            None => {
                unindexed.0 += 1;
                unindexed.1 += dir.size;
            },
        }
    }
    let db_missing = metas.len() - matched_metas;

    report_overview(&scan, total, loose, &fill, unknown_after);
    report_categories(&scan.cat, total);
    report_exts(&scan.ext, total);
    report_mode(&dist, total, unindexed);
    report_matrix(&dist);
    report_mode_star(&dist, total);
    report_tops(&mut scan.dirs, &metas, &mut scan.big);

    println!("\n== 一致性 ==");
    println!(
        "db 有而磁盘没有: {db_missing} 个顶级目录; 磁盘有而 db 没有: {} 个目录 ({})",
        unindexed.0,
        gib(unindexed.1)
    );
    if errors > 0 {
        println!("读取失败条目: {errors} (被占用/无权限, 其大小未计入)");
    }
    println!(
        "\n注: 模式/星级分布中，音频按引用难度均摊，其余文件按难度数均摊；星级为 db 值 + \
         rosu-pp(lazer 算法) 补算。"
    );
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
