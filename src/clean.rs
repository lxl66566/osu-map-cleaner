//! Beatmap set grouping from osu!.db, per-set matching and filesystem safety.

use std::{
    collections::HashMap,
    path::{Component, Path},
};

use crate::{
    db::Beatmap,
    expr::{Expr, MapInfo, Mode, Status},
};

/// How a beatmap set matches when its difficulties disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// The set matches if any difficulty matches.
    Any,
    /// The set matches only if every difficulty matches.
    All,
}

impl MatchMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Any => "任一难度匹配",
            Self::All => "全部难度匹配",
        }
    }
}

/// A beatmap set: every db entry sharing one Songs/ folder.
#[derive(Debug, Clone)]
pub struct BeatmapSet {
    pub folder: String,
    pub artist: String,
    pub title: String,
    pub creator: String,
    pub maps: Vec<MapInfo>,
}

impl BeatmapSet {
    #[must_use]
    pub fn matches(&self, expr: &Expr, mode: MatchMode) -> bool {
        let mut results = self.maps.iter().map(|m| expr.matches(m));
        match mode {
            MatchMode::Any => results.any(|b| b),
            MatchMode::All => results.all(|b| b),
        }
    }

    #[must_use]
    pub fn matched_count(&self, expr: &Expr) -> usize {
        self.maps.iter().filter(|m| expr.matches(m)).count()
    }

    #[must_use]
    pub fn display_name(&self) -> String {
        format!("{} - {} ({})", self.artist, self.title, self.creator)
    }
}

/// Group all beatmaps by Songs folder.
///
/// Returns the sets (in db order) plus the number of entries skipped because
/// they carry no folder name.
#[must_use]
pub fn group_sets(beatmaps: &[Beatmap]) -> (Vec<BeatmapSet>, usize) {
    let mut sets: Vec<BeatmapSet> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut skipped = 0;
    for b in beatmaps {
        let Some(folder) = b.folder_name.as_deref().filter(|s| !s.is_empty()) else {
            skipped += 1;
            continue;
        };
        let info = to_info(b);
        if let Some(&i) = index.get(folder) {
            sets[i].maps.push(info);
        } else {
            index.insert(folder.to_owned(), sets.len());
            sets.push(BeatmapSet {
                folder: folder.to_owned(),
                artist: b
                    .artist_ascii
                    .clone()
                    .or_else(|| b.artist_unicode.clone())
                    .unwrap_or_default(),
                title: b
                    .title_ascii
                    .clone()
                    .or_else(|| b.title_unicode.clone())
                    .unwrap_or_default(),
                creator: b.creator.clone().unwrap_or_default(),
                maps: vec![info],
            });
        }
    }
    (sets, skipped)
}

/// A db folder entry must be a pure relative path: at least one component,
/// every component `Normal`. Newer osu! versions store some sets in subfolders
/// (e.g. `<set>\mini`), so multiple levels are allowed; anything containing
/// `..`, `.`, a root, drive or UNC prefix is rejected so a corrupt or hostile
/// db entry can never point the deleter outside Songs/.
#[must_use]
pub fn is_safe_folder_path(name: &str) -> bool {
    let mut count = 0;
    for component in Path::new(name).components() {
        if !matches!(component, Component::Normal(_)) {
            return false;
        }
        count += 1;
    }
    count > 0
}

/// Total size in bytes of a directory tree. Unreadable entries count as 0;
/// symlinks are not followed. Cosmetic only (used in the preview).
#[must_use]
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += entry.metadata().map_or(0, |m| m.len());
        }
    }
    total
}

fn to_info(b: &Beatmap) -> MapInfo {
    MapInfo {
        mode: conv_mode(b.mode),
        status: conv_status(b.status),
        star: nomod_star(b),
        cs: f64::from(b.circle_size),
        ar: f64::from(b.approach_rate),
        od: f64::from(b.overall_difficulty),
        hp: f64::from(b.hp_drain),
        // osu!.db stores total time in milliseconds, drain time in seconds.
        length: f64::from(b.total_time) / 1000.0,
        drain: f64::from(b.drain_time),
    }
}

// db.rs rejects mode bytes > 3, so the catch-all is unreachable in practice.
fn conv_mode(mode: u8) -> Mode {
    match mode {
        0 => Mode::Standard,
        1 => Mode::Taiko,
        2 => Mode::Catch,
        _ => Mode::Mania,
    }
}

// Raw ranked-status values of osu!.db: 254..4 mean -2..4.
fn conv_status(status: u8) -> Status {
    match status {
        0 => Status::Pending,
        1 => Status::Ranked,
        2 => Status::Approved,
        3 => Status::Qualified,
        4 => Status::Loved,
        255 => Status::Unsubmitted,
        _ => Status::Unknown,
    }
}

/// Nomod star rating of the beatmap's own mode; falls back to the first
/// positive rating when the nomod entry is absent. `None` keeps star
/// conditions from ever matching (see [`MapInfo::star`]).
fn nomod_star(b: &Beatmap) -> Option<f64> {
    let ratings = &b.ratings[usize::from(b.mode)];
    ratings
        .iter()
        .find(|&(mods, _)| *mods == 0)
        .map(|&(_, sr)| sr)
        .filter(|&sr| sr > 0.0)
        .or_else(|| ratings.iter().map(|&(_, sr)| sr).find(|&sr| sr > 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_folder_paths() {
        for good in [
            "12345 artist - title",
            "a",
            "..a",
            "a b (c)",
            "123 x\\mini",
            "123/456",
        ] {
            assert!(is_safe_folder_path(good), "should be safe: {good:?}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "/a",
            "\\a",
            "a/../b",
            "C:\\a",
            "C:",
            "\\\\srv\\share",
        ] {
            assert!(!is_safe_folder_path(bad), "should be unsafe: {bad:?}");
        }
    }

    fn set_with_stars(stars: &[Option<f64>]) -> BeatmapSet {
        let mania_info = |star: Option<f64>| MapInfo {
            star,
            mode: Mode::Mania,
            status: Status::Ranked,
            cs: 7.0,
            ar: 9.5,
            od: 8.0,
            hp: 8.0,
            length: 90.0,
            drain: 80.0,
        };
        BeatmapSet {
            folder: "f".into(),
            artist: "a".into(),
            title: "t".into(),
            creator: "c".into(),
            maps: stars.iter().map(|&star| mania_info(star)).collect(),
        }
    }

    #[test]
    fn match_mode_any_vs_all() {
        let expr = Expr::parse("star<3").unwrap();
        let mixed = set_with_stars(&[Some(2.0), Some(5.0)]);
        assert!(mixed.matches(&expr, MatchMode::Any));
        assert!(!mixed.matches(&expr, MatchMode::All));
        assert_eq!(mixed.matched_count(&expr), 1);

        let easy = set_with_stars(&[Some(1.0), Some(2.5)]);
        assert!(easy.matches(&expr, MatchMode::Any));
        assert!(easy.matches(&expr, MatchMode::All));

        // unknown star counts as non-match in both modes
        let unknown = set_with_stars(&[None, Some(2.0)]);
        assert!(unknown.matches(&expr, MatchMode::Any));
        assert!(!unknown.matches(&expr, MatchMode::All));
    }
}
