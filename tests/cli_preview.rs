//! CLI integration tests on a fake library: db parsing, per-difficulty
//! planning (md5 verification, folder-empty detection) and preview output.
//! Runs with --dry-run only, so nothing is ever deleted.

// Fixture serializer: narrowing casts are intentional here.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use md5::{Digest, Md5};

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

fn star_list(out: &mut Vec<u8>, pairs: &[(u32, f64)]) {
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for &(mods, stars) in pairs {
        out.push(0x08);
        out.extend_from_slice(&mods.to_le_bytes());
        out.push(0x0c);
        out.extend_from_slice(&(stars as f32).to_le_bytes());
    }
}

/// One beatmap entry: (folder, .osu file name, md5, star rating).
fn beatmap_body(out: &mut Vec<u8>, (folder, file, md5, star): &(String, String, String, f64)) {
    put_string(out, Some("artist"));
    put_string(out, None);
    put_string(out, Some("title"));
    put_string(out, None);
    put_string(out, Some("creator"));
    put_string(out, Some("Diff"));
    put_string(out, Some("audio.mp3"));
    put_string(out, Some(md5));
    put_string(out, Some(file));
    out.push(4);
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&9.0f32.to_le_bytes());
    out.extend_from_slice(&4.0f32.to_le_bytes());
    out.extend_from_slice(&8.0f32.to_le_bytes());
    out.extend_from_slice(&8.0f32.to_le_bytes());
    out.extend_from_slice(&1.0f64.to_le_bytes());
    star_list(out, &[(0, *star)]);
    star_list(out, &[]);
    star_list(out, &[]);
    star_list(out, &[]);
    out.extend_from_slice(&80u32.to_le_bytes());
    out.extend_from_slice(&90_500u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&1i32.to_le_bytes());
    out.extend_from_slice(&42i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&0i16.to_le_bytes());
    out.extend_from_slice(&0.0f32.to_le_bytes());
    out.push(0);
    put_string(out, None);
    put_string(out, None);
    out.extend_from_slice(&0i16.to_le_bytes());
    put_string(out, None);
    out.push(0);
    out.extend_from_slice(&0u64.to_le_bytes());
    out.push(0);
    put_string(out, Some(folder));
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&[0; 5]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.push(0);
}

fn build_db(entries: &[(String, String, String, f64)]) -> Vec<u8> {
    let mut db = Vec::new();
    db.extend_from_slice(&20_260_711u32.to_le_bytes());
    db.extend_from_slice(&1u32.to_le_bytes());
    db.push(1);
    db.extend_from_slice(&0u64.to_le_bytes());
    put_string(&mut db, Some("player"));
    db.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        beatmap_body(&mut db, e);
    }
    db.extend_from_slice(&1u32.to_le_bytes());
    db
}

fn md5_of_file(path: &Path) -> String {
    use hex_simd::{AsciiCase, encode_to_string};
    encode_to_string(
        Md5::digest(std::fs::read(path).unwrap().as_slice()),
        AsciiCase::Lower,
    )
}

/// Fake library with three sets under `Songs/`:
/// - A: a1.osu (star 2, matched) + a2.osu (star 5, unmatched) + asset
/// - B: b1.osu with a stale md5 in the db → skipped by verification
/// - C: single matched difficulty c1.osu + asset → folder would be emptied
fn fake_library() -> PathBuf {
    let root = tempfile::tempdir().unwrap().keep();
    let songs = root.join("Songs");
    for d in ["A", "B", "C"] {
        std::fs::create_dir_all(songs.join(d)).unwrap();
    }
    std::fs::write(songs.join("A/a1.osu"), b"a1").unwrap();
    std::fs::write(songs.join("A/a2.osu"), b"a2").unwrap();
    std::fs::write(songs.join("A/audio.mp3"), b"audio").unwrap();
    std::fs::write(songs.join("B/b1.osu"), b"b1-tampered").unwrap();
    std::fs::write(songs.join("C/c1.osu"), b"c1").unwrap();
    std::fs::write(songs.join("C/video.mp4"), b"video").unwrap();
    let entries = vec![
        ("A", "a1.osu", md5_of_file(&songs.join("A/a1.osu")), 2.0),
        ("A", "a2.osu", md5_of_file(&songs.join("A/a2.osu")), 5.0),
        ("B", "b1.osu", "0".repeat(32), 2.0),
        ("C", "c1.osu", md5_of_file(&songs.join("C/c1.osu")), 2.0),
    ];
    let entries: Vec<_> = entries
        .into_iter()
        .map(|(a, b, c, d)| (a.into(), b.into(), c, d))
        .collect();
    std::fs::write(root.join("osu!.db"), build_db(&entries)).unwrap();
    root
}

fn run(root: &Path, extra: &[&str]) -> (String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_osu-map-cleaner"))
        .args(["star<3", "-d"])
        .arg(root)
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Answer the confirmation prompt in case one is reached; --dry-run
    // exits before it, so this only guards against accidental deletion.
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn default_mode_plans_partial_and_full_set_removals() {
    let root = fake_library();
    // No --target: per-difficulty deletion is the default.
    let (stdout, stderr) = run(&root, &["--dry-run"]);
    // Only the verifiable matched files count: A/a1 and C/c1.
    assert!(
        stdout.contains("匹配 2 / 3 个谱面集，将删除 2 个难度文件"),
        "{stdout}"
    );
    assert!(stdout.contains("2 个难度, 删 1 个"), "{stdout}");
    assert!(stdout.contains("1 个难度, 删 1 个（整组移除）"), "{stdout}");
    assert!(
        stderr.contains("1 个难度文件内容与数据库记录不符"),
        "{stderr}"
    );
    assert!(
        stdout.contains("--dry-run: 仅预览，未删除任何内容。"),
        "{stdout}"
    );
    // Nothing was touched.
    assert!(root.join("Songs/A/a1.osu").is_file());
    assert!(root.join("Songs/C").is_dir());
}

#[test]
fn set_mode_matches_whole_folders() {
    let root = fake_library();
    let (stdout, _stderr) = run(&root, &["--target", "set", "--dry-run"]);
    // Set mode ignores md5 and matches all three folders.
    assert!(stdout.contains("匹配 3 / 3 个谱面集"), "{stdout}");
}
