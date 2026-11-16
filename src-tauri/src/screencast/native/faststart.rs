//! "Fast start": put an mp4's index (`moov`) in front of its data (`mdat`), so that
//! a player can begin before the whole file has arrived. The ffmpeg path does this
//! with `+faststart`. The Windows sink writer cannot (see `mf.rs`), so it is done
//! on the finished file.
//!
//! Moving `moov` in front of `mdat` moves the data later by the size of `moov`, so
//! every chunk offset in it (`stco`, or `co64`) grows by that much. The data itself
//! is copied as it is.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The boxes that hold other boxes, down to the one with the chunk offsets.
const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

struct Top {
    kind: [u8; 4],
    start: u64,
    size: u64,
}

fn io_text(e: io::Error) -> String {
    e.to_string()
}

/// The boxes at the top level of the file, in order.
fn top_boxes(file: &mut File) -> Result<Vec<Top>, String> {
    let len = file.metadata().map_err(io_text)?.len();
    let mut boxes = Vec::new();
    let mut at = 0u64;
    while at < len {
        file.seek(SeekFrom::Start(at)).map_err(io_text)?;
        let mut head = [0u8; 8];
        file.read_exact(&mut head).map_err(io_text)?;
        let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        if size == 1 {
            let mut wide = [0u8; 8];
            file.read_exact(&mut wide).map_err(io_text)?;
            size = u64::from_be_bytes(wide);
        } else if size == 0 {
            size = len - at;
        }
        if size < 8 || at + size > len {
            return Err(format!(
                "a box at byte {at} has a size of {size}: not an mp4"
            ));
        }
        boxes.push(Top {
            kind: [head[4], head[5], head[6], head[7]],
            start: at,
            size,
        });
        at += size;
    }
    Ok(boxes)
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Add `by` to every chunk offset in the boxes of `bytes` (the inside of `moov`).
fn shift_offsets(bytes: &mut [u8], by: u64) -> Result<(), String> {
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let size = be32(bytes, at).ok_or("a box is cut off")? as usize;
        if size < 8 || at + size > bytes.len() {
            return Err("a box inside moov has a wrong size".to_string());
        }
        let kind: [u8; 4] = bytes[at + 4..at + 8].try_into().expect("4 bytes");
        let body = at + 8..at + size;
        if CONTAINERS.contains(&&kind) {
            shift_offsets(&mut bytes[body], by)?;
        } else if &kind == b"stco" || &kind == b"co64" {
            let wide = &kind == b"co64";
            let width = if wide { 8 } else { 4 };
            let entries = &mut bytes[body];
            // 1 byte of version and 3 of flags, then the number of entries.
            let count = be32(entries, 4).ok_or("a chunk table is cut off")? as usize;
            if entries.len() < 8 + count * width {
                return Err("a chunk table is shorter than its count".to_string());
            }
            for i in 0..count {
                let place = 8 + i * width;
                if wide {
                    let old = u64::from_be_bytes(entries[place..place + 8].try_into().expect("8"));
                    entries[place..place + 8].copy_from_slice(&(old + by).to_be_bytes());
                } else {
                    let old = u64::from(be32(entries, place).expect("inside the table"));
                    let new = u32::try_from(old + by)
                        .map_err(|_| "a chunk offset does not fit 32 bits".to_string())?;
                    entries[place..place + 4].copy_from_slice(&new.to_be_bytes());
                }
            }
        }
        at += size;
    }
    Ok(())
}

/// Move `moov` in front of `mdat`. A file that already has it there is left
/// alone. On an error the file is untouched.
pub fn faststart(path: &Path) -> Result<(), String> {
    let mut input = File::open(path).map_err(io_text)?;
    let boxes = top_boxes(&mut input)?;
    let mdat = boxes.iter().position(|b| &b.kind == b"mdat");
    let moov = boxes.iter().position(|b| &b.kind == b"moov");
    let (Some(mdat), Some(moov)) = (mdat, moov) else {
        return Err("the file has no mdat and moov boxes: not an mp4".to_string());
    };
    if moov < mdat {
        return Ok(());
    }

    let index = &boxes[moov];
    let mut moov_bytes = vec![0u8; usize::try_from(index.size).map_err(|e| e.to_string())?];
    input.seek(SeekFrom::Start(index.start)).map_err(io_text)?;
    input.read_exact(&mut moov_bytes).map_err(io_text)?;
    if be32(&moov_bytes, 0) == Some(1) {
        return Err("a moov box with a 64-bit size is not handled".to_string());
    }
    shift_offsets(&mut moov_bytes[8..], index.size)?;

    let mut temp: PathBuf = path.into();
    temp.set_extension("faststart.part");
    let written = (|| -> Result<(), String> {
        let mut output = File::create(&temp).map_err(io_text)?;
        for (i, b) in boxes.iter().enumerate() {
            if i == moov {
                continue;
            }
            if i == mdat {
                output.write_all(&moov_bytes).map_err(io_text)?;
            }
            input.seek(SeekFrom::Start(b.start)).map_err(io_text)?;
            let copied = io::copy(&mut (&mut input).take(b.size), &mut output).map_err(io_text)?;
            if copied != b.size {
                return Err("the file ended inside a box".to_string());
            }
        }
        output.flush().map_err(io_text)
    })();
    drop(input);
    match written {
        // `rename` replaces the file, also on Windows.
        Ok(()) => std::fs::rename(&temp, path).map_err(|e| {
            let _ = std::fs::remove_file(&temp);
            io_text(e)
        }),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    fn chunk_table(kind: &[u8; 4], offsets: &[u64]) -> Vec<u8> {
        let mut payload = vec![0, 0, 0, 0];
        payload.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for &o in offsets {
            if kind == b"co64" {
                payload.extend_from_slice(&o.to_be_bytes());
            } else {
                payload.extend_from_slice(&(o as u32).to_be_bytes());
            }
        }
        boxed(kind, &payload)
    }

    fn movie(table: Vec<u8>) -> Vec<u8> {
        // A sibling box next to the table, so the walk has to step over boxes.
        let stbl = boxed(b"stbl", &[boxed(b"stsd", &[0; 8]), table].concat());
        let trak = boxed(b"trak", &boxed(b"mdia", &boxed(b"minf", &stbl)));
        boxed(b"moov", &[boxed(b"mvhd", &[0; 100]), trak].concat())
    }

    /// ftyp, then the data, then the index that points into the data.
    fn index_last(kind: &[u8; 4]) -> (Vec<u8>, Vec<u8>) {
        let ftyp = boxed(b"ftyp", b"isom0000isom");
        let data: Vec<u8> = (0..200u8).collect();
        let first = ftyp.len() as u64 + 8;
        let table = chunk_table(kind, &[first, first + 50, first + 120]);
        let file = [ftyp, boxed(b"mdat", &data), movie(table)].concat();
        (file, data)
    }

    fn kinds(file: &[u8]) -> Vec<String> {
        let mut at = 0;
        let mut out = Vec::new();
        while at < file.len() {
            let size = be32(file, at).unwrap() as usize;
            out.push(String::from_utf8_lossy(&file[at + 4..at + 8]).to_string());
            at += size;
        }
        out
    }

    fn offsets_in(file: &[u8], kind: &[u8; 4]) -> Vec<u64> {
        let at = file.windows(4).position(|w| w == kind).unwrap() + 4;
        let count = be32(file, at + 4).unwrap() as usize;
        (0..count)
            .map(|i| {
                if kind == b"co64" {
                    u64::from_be_bytes(file[at + 8 + i * 8..at + 16 + i * 8].try_into().unwrap())
                } else {
                    u64::from(be32(file, at + 8 + i * 4).unwrap())
                }
            })
            .collect()
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dalzzack-faststart-{}-{name}.mp4",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_index_moves_in_front_of_the_data_and_the_offsets_follow_it() {
        for kind in [b"stco", b"co64"] {
            let (file, data) = index_last(kind);
            assert_eq!(kinds(&file), ["ftyp", "mdat", "moov"]);
            let moov_len = movie(chunk_table(kind, &[0, 0, 0])).len() as u64;
            let before = offsets_in(&file, kind);

            let path = temp_file(&format!("move-{}", kind[1]), &file);
            faststart(&path).unwrap();
            let after = std::fs::read(&path).unwrap();
            let _ = std::fs::remove_file(&path);

            assert_eq!(kinds(&after), ["ftyp", "moov", "mdat"]);
            assert_eq!(after.len(), file.len(), "nothing is added or lost");
            let moved = offsets_in(&after, kind);
            assert_eq!(
                moved,
                before.iter().map(|o| o + moov_len).collect::<Vec<_>>(),
                "{kind:?}"
            );
            // Each offset still points at the same byte of the data as before.
            for (old, new) in before.iter().zip(&moved) {
                let wanted = file[*old as usize];
                assert_eq!(after[*new as usize], wanted);
            }
            let start = moved[0] as usize;
            assert_eq!(&after[start..start + 50], &data[..50]);
        }
    }

    #[test]
    fn a_file_that_has_its_index_first_is_left_as_it_is() {
        let (file, _) = index_last(b"stco");
        let path = temp_file("twice", &file);
        faststart(&path).unwrap();
        let once = std::fs::read(&path).unwrap();
        faststart(&path).unwrap();
        let twice = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_file_that_is_not_an_mp4_is_an_error_and_stays_untouched() {
        let path = temp_file("garbage", b"this is not an mp4 file at all");
        assert!(faststart(&path).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"this is not an mp4 file at all"
        );
        let part = path.with_extension("faststart.part");
        assert!(!part.exists(), "no temporary file is left");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_chunk_table_that_would_overflow_32_bits_leaves_the_file_as_it_was() {
        let ftyp = boxed(b"ftyp", b"isom0000isom");
        let table = chunk_table(b"stco", &[u64::from(u32::MAX) - 5]);
        let file = [ftyp, boxed(b"mdat", &[1, 2, 3]), movie(table)].concat();
        let path = temp_file("overflow", &file);
        assert!(faststart(&path).unwrap_err().contains("32 bits"));
        assert_eq!(std::fs::read(&path).unwrap(), file);
        let _ = std::fs::remove_file(&path);
    }
}
