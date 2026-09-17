//! Local star rating backfill for difficulties whose osu!.db entry has no
//! value. osu! computes std star ratings lazily, so sets imported in bulk
//! carry no stars at all; rosu-pp computes them offline instead.
//!
//! Safety mirrors `clean::nomod_star`: only positive values count as a star,
//! and any difficulty that cannot be resolved keeps `star = None`, which
//! star conditions never match. `checked_calculate` additionally rejects
//! maps flagged as suspicious (corrupt files) instead of yielding garbage
//! stars that could cause wrong deletions.

use std::{
    ops::AddAssign,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};

use rayon::prelude::{IntoParallelRefMutIterator, ParallelIterator};
use rosu_pp::{Beatmap, Difficulty, model::mode::GameMode};

use crate::{
    clean::{self, BeatmapSet},
    expr::Mode,
};

/// Print a progress line every that many sets.
const PROGRESS_EVERY: usize = 2_000;

/// Fill outcomes over all difficulties that lacked a star.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StarStats {
    /// Stars computed and filled in.
    pub computed: usize,
    /// The db entry carries no .osu file name.
    pub no_file_name: usize,
    /// Folder or file name is not a safe relative path under Songs/.
    pub unsafe_name: usize,
    /// The .osu file does not exist on disk.
    pub missing_file: usize,
    /// File unreadable, mode-mismatched, empty, rejected as suspicious, or
    /// the computed stars are non-positive.
    pub calc_failed: usize,
}

impl AddAssign for StarStats {
    fn add_assign(&mut self, rhs: Self) {
        self.computed += rhs.computed;
        self.no_file_name += rhs.no_file_name;
        self.unsafe_name += rhs.unsafe_name;
        self.missing_file += rhs.missing_file;
        self.calc_failed += rhs.calc_failed;
    }
}

/// Fill in `info.star` for every difficulty whose value is `None`, in
/// parallel over sets. Difficulties that already have a star are skipped;
/// ones that cannot be resolved stay `None` (star conditions never match).
///
/// The values follow osu!lazer's difficulty algorithm, which may differ
/// from what stable displays by a few tenths of a star.
pub fn fill_missing_stars(sets: &mut [BeatmapSet], songs_dir: &Path) -> StarStats {
    let total = sets.len();
    let done = AtomicUsize::new(0);
    sets.par_iter_mut()
        .map(|set| {
            let stats = fill_set(set, songs_dir);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % PROGRESS_EVERY == 0 {
                println!("星数补算进度: {n}/{total} 个谱面集");
            }
            stats
        })
        .reduce(StarStats::default, |mut acc, s| {
            acc += s;
            acc
        })
}

fn fill_set(set: &mut BeatmapSet, songs_dir: &Path) -> StarStats {
    let mut stats = StarStats::default();
    let folder_ok = clean::is_safe_rel_path(&set.folder);
    for d in &mut set.maps {
        if d.info.star.is_some() {
            continue;
        }
        let Some(name) = d.osu_file.as_deref() else {
            stats.no_file_name += 1;
            continue;
        };
        if !folder_ok || !clean::is_safe_rel_path(name) {
            stats.unsafe_name += 1;
            continue;
        }
        let path = songs_dir.join(&set.folder).join(name);
        if !path.is_file() {
            stats.missing_file += 1;
            continue;
        }
        let Ok(map) = Beatmap::from_path(&path) else {
            stats.calc_failed += 1;
            continue;
        };
        match stars_of(&map, d.info.mode) {
            Some(sr) if sr > 0.0 => {
                d.info.star = Some(sr);
                stats.computed += 1;
            },
            _ => stats.calc_failed += 1,
        }
    }
    stats
}

/// Nomod stars in the difficulty's own mode. rosu-pp only converts std
/// files to other modes; any other mismatch between the db entry's mode
/// and the file's mode is a data inconsistency and yields `None`.
fn stars_of(map: &Beatmap, mode: Mode) -> Option<f64> {
    // An empty map (corrupt file / downloader residue) parses fine but
    // yields a meaningless base star value (~0.14) — treat as unknown.
    if map.hit_objects.is_empty() {
        return None;
    }
    let target = game_mode(mode);
    if target != map.mode && map.mode != GameMode::Osu {
        return None;
    }
    let diff = Difficulty::new();
    let stars = match target {
        GameMode::Osu => {
            diff.checked_calculate_for_mode::<rosu_pp::osu::Osu>(map)
                .ok()?
                .stars
        },
        GameMode::Taiko => {
            diff.checked_calculate_for_mode::<rosu_pp::taiko::Taiko>(map)
                .ok()?
                .stars
        },
        GameMode::Catch => {
            diff.checked_calculate_for_mode::<rosu_pp::catch::Catch>(map)
                .ok()?
                .stars
        },
        GameMode::Mania => {
            diff.checked_calculate_for_mode::<rosu_pp::mania::Mania>(map)
                .ok()?
                .stars
        },
    };
    Some(stars)
}

fn game_mode(mode: Mode) -> GameMode {
    match mode {
        Mode::Standard => GameMode::Osu,
        Mode::Taiko => GameMode::Taiko,
        Mode::Catch => GameMode::Catch,
        Mode::Mania => GameMode::Mania,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clean::Difficulty,
        expr::{MapInfo, Status},
    };

    /// std map with big spaced jumps: guaranteed positive stars, also as a
    /// mania conversion.
    const OSU_FILE: &str =
        "osu file format v14\n\n[General]\nAudioFilename: audio.mp3\nMode: \
         0\n\n[Metadata]\nTitleUnicode: t\nArtistUnicode: a\nCreator: c\nVersion: v\nBeatmapID: \
         1\nBeatmapSetID: 1\n\n[Difficulty]\nHPDrainRate: 5\nCircleSize: 4\nOverallDifficulty: \
         8\nApproachRate: 9\nSliderMultiplier: 1.4\nSliderTickRate: \
         1\n\n[TimingPoints]\n0,300,4,1,0,100,1,0\n\n[HitObjects]\n0,192,0,1,0,0:0:0:0:\n512,192,\
         150,1,0,0:0:0:0:\n0,192,300,1,0,0:0:0:0:\n512,192,450,1,0,0:0:0:0:\n0,192,600,1,0,0:0:0:\
         0:\n512,192,750,1,0,0:0:0:0:\n0,192,900,1,0,0:0:0:0:\n512,192,1050,1,0,0:0:0:0:\n";

    /// 4K mania map: x positions map to columns (col * 512 / 4).
    const MANIA_FILE: &str =
        "osu file format v14\n\n[General]\nAudioFilename: audio.mp3\nMode: \
         3\n\n[Metadata]\nTitleUnicode: t\nArtistUnicode: a\nCreator: c\nVersion: v\nBeatmapID: \
         1\nBeatmapSetID: 1\n\n[Difficulty]\nHPDrainRate: 8\nCircleSize: 4\nOverallDifficulty: \
         8\nApproachRate: 9\nSliderMultiplier: 1.4\nSliderTickRate: \
         1\n\n[TimingPoints]\n0,300,4,1,0,100,1,0\n\n[HitObjects]\n64,192,0,1,0,0:0:0:0:\n192,192,\
         150,1,0,0:0:0:0:\n320,192,300,1,0,0:0:0:0:\n448,192,450,1,0,0:0:0:0:\n64,192,600,1,0,0:0:\
         0:0:\n192,192,750,1,0,0:0:0:0:\n320,192,900,1,0,0:0:0:0:\n448,192,1050,1,0,0:0:0:0:\n";

    fn set_with(name: Option<&str>, mode: Mode) -> BeatmapSet {
        let info = MapInfo {
            mode,
            status: Status::Ranked,
            star: None,
            cs: 4.0,
            ar: 9.0,
            od: 8.0,
            hp: 5.0,
            length: 60.0,
            drain: 50.0,
        };
        BeatmapSet {
            folder: "set".into(),
            artist: "a".into(),
            title: "t".into(),
            creator: "c".into(),
            maps: vec![Difficulty {
                info,
                osu_file: name.map(str::to_owned),
                md5: None,
            }],
        }
    }

    #[test]
    fn computes_native_and_converted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("set")).unwrap();
        std::fs::write(dir.path().join("set").join("a.osu"), OSU_FILE).unwrap();

        let mut sets = vec![
            set_with(Some("a.osu"), Mode::Standard),
            set_with(Some("a.osu"), Mode::Mania),
        ];
        let stats = fill_missing_stars(&mut sets, dir.path());
        assert_eq!(stats.computed, 2);
        for set in &sets {
            assert!(set.maps[0].info.star.is_some_and(|sr| sr > 0.0));
        }

        // already-known stars are left untouched and not re-counted
        let stats = fill_missing_stars(&mut sets, dir.path());
        assert_eq!(stats, StarStats::default());
    }

    #[test]
    fn unresolvable_stays_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("set")).unwrap();
        std::fs::write(dir.path().join("set").join("garbage.osu"), b"not a beatmap").unwrap();
        let mut sets = vec![
            set_with(None, Mode::Standard),                // no file name
            set_with(Some("missing.osu"), Mode::Standard), // not on disk
            set_with(Some("../evil.osu"), Mode::Standard), // unsafe path
            set_with(Some("garbage.osu"), Mode::Standard), // unparseable
        ];
        let stats = fill_missing_stars(&mut sets, dir.path());
        assert_eq!(stats.no_file_name, 1);
        assert_eq!(stats.missing_file, 1);
        assert_eq!(stats.unsafe_name, 1);
        assert_eq!(stats.calc_failed, 1);
        assert_eq!(stats.computed, 0);
        assert!(sets.iter().all(|s| s.maps[0].info.star.is_none()));
    }

    #[test]
    fn mode_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.osu"), MANIA_FILE).unwrap();
        let map = Beatmap::from_path(dir.path().join("m.osu")).unwrap();
        assert_eq!(map.mode, GameMode::Mania);
        assert!(stars_of(&map, Mode::Mania).is_some());
        // mania -> std is not a supported conversion, must yield None
        assert!(stars_of(&map, Mode::Standard).is_none());
    }
}
