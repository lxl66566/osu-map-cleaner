//! Minimal osu!.db reader.
//!
//! Field order follows the classic format (as implemented by the osu-db
//! crate). The only modern deviation found so far — db version 20260711,
//! verified against a real library — is star rating values stored as f32
//! with tag 0x0c instead of f64 with tag 0x0d; both layouts are accepted.
//! The parser is strict: any unexpected byte fails loudly instead of
//! producing wrong data that could lead to wrong deletions.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("数据不足: 需要 {need} 字节仅剩 {left} (offset {pos:#x})")]
    UnexpectedEof {
        need: usize,
        left: usize,
        pos: usize,
    },
    #[error("非法字符串标志 0x{flag:02x} (offset {pos:#x})，数据库格式可能已变化")]
    BadStringFlag { flag: u8, pos: usize },
    #[error("非法星数标签 0x{tag:02x} (offset {pos:#x})，数据库格式可能已变化")]
    BadStarTag { tag: u8, pos: usize },
    #[error("非法数量 {count} (offset {pos:#x})，数据库格式可能已变化")]
    BadCount { count: i64, pos: usize },
    #[error("非法模式 {mode} (offset {pos:#x})")]
    BadMode { mode: u8, pos: usize },
    #[error("解析后仍有 {left} 字节未消费，数据库格式可能已变化")]
    TrailingBytes { left: usize },
}

#[derive(Debug)]
pub struct Db {
    pub version: u32,
    pub beatmaps: Vec<Beatmap>,
}

/// One difficulty entry; only fields consumed by the cleaner are kept.
#[derive(Debug)]
pub struct Beatmap {
    pub artist_ascii: Option<String>,
    pub artist_unicode: Option<String>,
    pub title_ascii: Option<String>,
    pub title_unicode: Option<String>,
    pub creator: Option<String>,
    /// The difficulty's audio file name, as recorded in the db.
    pub audio: Option<String>,
    pub status: u8,
    pub mode: u8,
    pub circle_size: f32,
    pub approach_rate: f32,
    pub overall_difficulty: f32,
    pub hp_drain: f32,
    /// Per-mode star ratings `[std, taiko, ctb, mania]` as `(mod bits, stars)`.
    pub ratings: [Vec<(u32, f64)>; 4],
    /// Drain time in seconds.
    pub drain_time: u32,
    /// Total time in milliseconds.
    pub total_time: u32,
    /// Content md5 of the difficulty's .osu file, hex-encoded.
    pub md5: Option<String>,
    pub folder_name: Option<String>,
    /// The difficulty's .osu file name, relative to the set folder.
    pub file_name: Option<String>,
}

// Version thresholds of the format, as used by osu-db.
const V_FLOAT_DIFFS: u32 = 20_140_609; // difficulties become f32, star lists exist
const V_NO_ENTRY_SIZE: u32 = 20_191_106; // per-entry byte-size prefix dropped

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let left = self.data.len() - self.pos;
        if n > left {
            return Err(Error::UnexpectedEof {
                need: n,
                left,
                pos: self.pos,
            });
        }
        let bytes = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.take(2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        self.take(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn f32(&mut self) -> Result<f32, Error> {
        self.take(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
    }

    fn f64(&mut self) -> Result<f64, Error> {
        self.take(8)
            .map(|b| f64::from_le_bytes(b.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<Option<String>, Error> {
        match self.u8()? {
            0x00 => Ok(None),
            0x0b => {
                let len = self.uleb128()?;
                let len = usize::try_from(len).map_err(|_| Error::BadCount {
                    count: i64::try_from(len).unwrap_or(-1),
                    pos: self.pos,
                })?;
                let bytes = self.take(len)?;
                Ok(Some(String::from_utf8_lossy(bytes).into_owned()))
            },
            flag => Err(Error::BadStringFlag {
                flag,
                pos: self.pos - 1,
            }),
        }
    }

    fn uleb128(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            value |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                return Err(Error::BadCount {
                    count: -1,
                    pos: self.pos,
                });
            }
        }
    }

    /// Length prefix with a sanity bound, so a mis-parsed count cannot make
    /// the reader allocate gigabytes or run off the file.
    fn count(&mut self, max: usize) -> Result<usize, Error> {
        let n = i64::from(self.i32()?);
        if !(0..=i64::try_from(max).unwrap_or(i64::MAX)).contains(&n) {
            return Err(Error::BadCount {
                count: n,
                pos: self.pos - 4,
            });
        }
        // Range-checked above; the cast can neither truncate nor lose sign.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(n as usize)
    }

    fn beatmap(&mut self, version: u32) -> Result<Beatmap, Error> {
        if version < V_NO_ENTRY_SIZE {
            let _entry_size = self.u32()?;
        }
        let artist_ascii = self.string()?;
        let artist_unicode = self.string()?;
        let title_ascii = self.string()?;
        let title_unicode = self.string()?;
        let creator = self.string()?;
        let _difficulty_name = self.string()?;
        let audio = self.string()?;
        let md5 = self.string()?;
        let file_name = self.string()?;
        let status = self.u8()?;
        let _circles = self.u16()?;
        let _sliders = self.u16()?;
        let _spinners = self.u16()?;
        let _last_modified = self.u64()?;
        let (approach_rate, circle_size, hp_drain, overall_difficulty) = if version >= V_FLOAT_DIFFS
        {
            (self.f32()?, self.f32()?, self.f32()?, self.f32()?)
        } else {
            (
                f32::from(self.u8()?),
                f32::from(self.u8()?),
                f32::from(self.u8()?),
                f32::from(self.u8()?),
            )
        };
        let _slider_velocity = self.f64()?;
        let ratings = if version >= V_FLOAT_DIFFS {
            [
                self.star_list()?,
                self.star_list()?,
                self.star_list()?,
                self.star_list()?,
            ]
        } else {
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()]
        };
        let drain_time = self.u32()?;
        let total_time = self.u32()?;
        let _preview_time = self.u32()?;
        let timing_points = self.count(100_000)?;
        for _ in 0..timing_points {
            let _bpm = self.f64()?;
            let _offset = self.f64()?;
            let _inherits = self.u8()?;
        }
        let _beatmap_id = self.i32()?;
        let _beatmapset_id = self.i32()?;
        let _thread_id = self.u32()?;
        for _ in 0..4 {
            let _grade = self.u8()?;
        }
        let _local_offset = self.u16()?;
        let _stack_leniency = self.f32()?;
        let mode = self.u8()?;
        if mode > 3 {
            return Err(Error::BadMode {
                mode,
                pos: self.pos - 1,
            });
        }
        let _source = self.string()?;
        let _tags = self.string()?;
        let _online_offset = self.u16()?;
        let _font = self.string()?;
        let _unplayed = self.u8()?;
        let _last_played = self.u64()?;
        let _is_osz2 = self.u8()?;
        let folder_name = self.string()?;
        let _last_online_check = self.u64()?;
        for _ in 0..5 {
            let _flag = self.u8()?;
        }
        if version < V_FLOAT_DIFFS {
            let _mysterious_short = self.u16()?;
        }
        let _last_edit = self.u32()?;
        let _mania_scroll_speed = self.u8()?;
        Ok(Beatmap {
            artist_ascii,
            artist_unicode,
            title_ascii,
            title_unicode,
            creator,
            audio,
            status,
            mode,
            circle_size,
            approach_rate,
            overall_difficulty,
            hp_drain,
            ratings,
            drain_time,
            total_time,
            md5,
            folder_name,
            file_name,
        })
    }

    /// One star rating list: count, then `(0x08, mods:u32, tag, value)` pairs
    /// where tag 0x0d means f64 (old) and 0x0c means f32 (2026+).
    fn star_list(&mut self) -> Result<Vec<(u32, f64)>, Error> {
        let count = self.count(10_000)?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let tag1 = self.u8()?;
            if tag1 != 0x08 {
                return Err(Error::BadStarTag {
                    tag: tag1,
                    pos: self.pos - 1,
                });
            }
            let mods = self.u32()?;
            let tag2 = self.u8()?;
            let stars = match tag2 {
                0x0d => self.f64()?,
                0x0c => f64::from(self.f32()?),
                tag => {
                    return Err(Error::BadStarTag {
                        tag,
                        pos: self.pos - 1,
                    })
                },
            };
            out.push((mods, stars));
        }
        Ok(out)
    }
}

/// Parse the whole `osu!.db` file content.
///
/// # Errors
/// Returns [`Error`] when the content does not follow a known layout;
/// in that case nothing should be deleted.
pub fn parse(data: &[u8]) -> Result<Db, Error> {
    let mut r = Reader { data, pos: 0 };
    let version = r.u32()?;
    let _folder_count = r.u32()?;
    let _account_unlocked = r.u8()?;
    let _date_unlocked = r.u64()?;
    let _player_name = r.string()?;
    let beatmap_count = r.count(10_000_000)?;
    let mut beatmaps = Vec::with_capacity(beatmap_count);
    for _ in 0..beatmap_count {
        beatmaps.push(r.beatmap(version)?);
    }
    let _user_permissions = r.u32()?;
    if r.pos != data.len() {
        return Err(Error::TrailingBytes {
            left: data.len() - r.pos,
        });
    }
    Ok(Db { version, beatmaps })
}

#[cfg(test)]
mod tests {
    // Fixture serializer: narrowing casts are intentional here.
    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::*;

    // -- tiny db serializer used to build test fixtures --

    fn put_string(out: &mut Vec<u8>, s: Option<&str>) {
        match s {
            None => out.push(0x00),
            Some(s) => {
                out.push(0x0b);
                let bytes = s.as_bytes();
                let mut len = bytes.len();
                loop {
                    let mut b = (len & 0x7f) as u8;
                    len >>= 7;
                    if len != 0 {
                        b |= 0x80;
                    }
                    out.push(b);
                    if len == 0 {
                        break;
                    }
                }
                out.extend_from_slice(bytes);
            },
        }
    }

    fn header(out: &mut Vec<u8>, version: u32, beatmaps: u32) {
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // folder count
        out.push(1); // account unlocked
        out.extend_from_slice(&0u64.to_le_bytes()); // date
        put_string(out, Some("player"));
        out.extend_from_slice(&beatmaps.to_le_bytes());
    }

    fn star_pair(out: &mut Vec<u8>, mods: u32, stars: f64, as_f32: bool) {
        out.push(0x08);
        out.extend_from_slice(&mods.to_le_bytes());
        if as_f32 {
            out.push(0x0c);
            out.extend_from_slice(&(stars as f32).to_le_bytes());
        } else {
            out.push(0x0d);
            out.extend_from_slice(&stars.to_le_bytes());
        }
    }

    fn star_list(out: &mut Vec<u8>, pairs: &[(u32, f64)], as_f32: bool) {
        out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        for &(mods, stars) in pairs {
            star_pair(out, mods, stars, as_f32);
        }
    }

    fn beatmap_body(out: &mut Vec<u8>, folder: &str, mode: u8, cs: f32, as_f32: bool) {
        put_string(out, Some("artist"));
        put_string(out, None);
        put_string(out, Some("title"));
        put_string(out, None);
        put_string(out, Some("creator"));
        put_string(out, Some("Diff"));
        put_string(out, Some("audio.mp3"));
        put_string(out, Some("md5"));
        put_string(out, Some("file.osu"));
        out.push(4); // status
        out.extend_from_slice(&0i16.to_le_bytes()); // circles
        out.extend_from_slice(&0i16.to_le_bytes()); // sliders
        out.extend_from_slice(&0i16.to_le_bytes()); // spinners
        out.extend_from_slice(&0u64.to_le_bytes()); // last modified
        out.extend_from_slice(&9.0f32.to_le_bytes()); // ar
        out.extend_from_slice(&cs.to_le_bytes());
        out.extend_from_slice(&8.0f32.to_le_bytes()); // hp
        out.extend_from_slice(&8.0f32.to_le_bytes()); // od
        out.extend_from_slice(&1.0f64.to_le_bytes()); // slider velocity
        star_list(out, &[(0, 2.5), (64, 3.5)], as_f32);
        star_list(out, &[], as_f32);
        star_list(out, &[], as_f32);
        star_list(out, &[(0, 1.5)], as_f32);
        out.extend_from_slice(&80u32.to_le_bytes()); // drain
        out.extend_from_slice(&90_500u32.to_le_bytes()); // total (ms)
        out.extend_from_slice(&0u32.to_le_bytes()); // preview
        out.extend_from_slice(&0u32.to_le_bytes()); // timing points
        out.extend_from_slice(&1i32.to_le_bytes()); // beatmap id
        out.extend_from_slice(&42i32.to_le_bytes()); // beatmapset id
        out.extend_from_slice(&0u32.to_le_bytes()); // thread id
        out.extend_from_slice(&[0, 0, 0, 0]); // grades
        out.extend_from_slice(&0i16.to_le_bytes()); // local offset
        out.extend_from_slice(&0.0f32.to_le_bytes()); // stack leniency
        out.push(mode);
        put_string(out, None); // source
        put_string(out, None); // tags
        out.extend_from_slice(&0i16.to_le_bytes()); // online offset
        put_string(out, None); // font
        out.push(0); // unplayed
        out.extend_from_slice(&0u64.to_le_bytes()); // last played
        out.push(0); // is osz2
        put_string(out, Some(folder));
        out.extend_from_slice(&0u64.to_le_bytes()); // last online check
        out.extend_from_slice(&[0; 5]); // flags
        out.extend_from_slice(&0u32.to_le_bytes()); // last edit
        out.push(0); // mania scroll
    }

    #[test]
    fn parse_modern_f32_stars() {
        let mut db = Vec::new();
        header(&mut db, 20_260_711, 1);
        beatmap_body(&mut db, "42 artist - title", 3, 7.0, true);
        db.extend_from_slice(&1u32.to_le_bytes()); // user permissions
        let parsed = parse(&db).unwrap();
        assert_eq!(parsed.version, 20_260_711);
        assert_eq!(parsed.beatmaps.len(), 1);
        let b = &parsed.beatmaps[0];
        assert_eq!(b.folder_name.as_deref(), Some("42 artist - title"));
        assert_eq!(b.file_name.as_deref(), Some("file.osu"));
        assert_eq!(b.md5.as_deref(), Some("md5"));
        assert_eq!(b.mode, 3);
        assert_eq!(b.circle_size, 7.0);
        assert_eq!(b.total_time, 90_500);
        assert_eq!(b.drain_time, 80);
        assert!((b.ratings[0][0].1 - 2.5).abs() < 1e-6);
        assert!((b.ratings[3][0].1 - 1.5).abs() < 1e-6);
    }

    #[test]
    fn parse_old_f64_stars_with_entry_size() {
        let mut db = Vec::new();
        header(&mut db, 20_191_105, 1);
        db.extend_from_slice(&0u32.to_le_bytes()); // entry size prefix
        beatmap_body(&mut db, "folder", 0, 4.0, false);
        db.extend_from_slice(&1u32.to_le_bytes());
        let parsed = parse(&db).unwrap();
        assert_eq!(parsed.beatmaps[0].ratings[0][0], (0, 2.5));
    }

    #[test]
    fn parse_rejects_garbage() {
        // truncated
        let mut db = Vec::new();
        header(&mut db, 20_260_711, 1);
        db.extend_from_slice(&[0x0b]);
        assert!(parse(&db).is_err());
        // trailing bytes
        let mut db = Vec::new();
        header(&mut db, 20_260_711, 0);
        db.extend_from_slice(&1u32.to_le_bytes());
        db.push(0xff);
        assert!(matches!(parse(&db), Err(Error::TrailingBytes { left: 1 })));
    }
}
