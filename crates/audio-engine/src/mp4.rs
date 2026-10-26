//! Minimal top-level ISO-BMFF parsing for forward-only native playback of
//! fragmented MP4 (YouTube DASH AAC, itag 140). Symphonia's seekable isomp4
//! path scans every top-level atom before returning, which over HTTP range
//! windows means downloading the whole file first. Instead the decoder is fed
//! `ftyp+moov` followed by the fragment containing the target time.
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub struct Sidx {
    pub timescale: u32,
    /// Absolute byte offset of the first referenced fragment.
    pub first: u64,
    /// Referenced fragment sizes (bytes) and durations (timescale ticks).
    pub references: Vec<(u64, u64)>,
}

#[derive(Debug, PartialEq)]
pub struct Layout {
    /// End of the `moov` box: bytes `[0, init_end)` initialise the decoder.
    pub init_end: u64,
    /// `moov` contains `mvex`, i.e. media lives in `moof`/`mdat` fragments.
    pub fragmented: bool,
    pub sidx: Option<Sidx>,
}

/// Box size, four-character type and header length at `offset`.
fn header(data: &[u8], offset: usize) -> Option<(u64, [u8; 4], usize)> {
    let bytes = data.get(offset..offset + 8)?;
    let size = u32::from_be_bytes(bytes[..4].try_into().ok()?) as u64;
    let kind: [u8; 4] = bytes[4..8].try_into().ok()?;
    match size {
        // Size 0 ("extends to EOF") never precedes the init data we need.
        0 => None,
        1 => Some((u64::from_be_bytes(data.get(offset + 8..offset + 16)?.try_into().ok()?), kind, 16))
            .filter(|(size, ..)| *size >= 16),
        8.. => Some((size, kind, 8)),
        _ => None,
    }
}

fn be(data: &[u8], at: usize, len: usize) -> Option<u64> {
    Some(data.get(at..at + len)?.iter().fold(0, |value, byte| value << 8 | u64::from(*byte)))
}

/// Parse a `sidx` payload (after the box header). `end` is the absolute
/// offset just past the box, the anchor for `first_offset`.
pub fn parse_sidx(body: &[u8], end: u64) -> Option<Sidx> {
    let version = *body.first()?;
    let timescale = be(body, 8, 4)? as u32;
    let (first_offset, rest) = match version {
        0 => (be(body, 16, 4)?, 20),
        1 => (be(body, 20, 8)?, 28),
        _ => return None,
    };
    let count = be(body, rest + 2, 2)? as usize;
    let mut references = Vec::with_capacity(count);
    for index in 0..count {
        let at = rest + 4 + index * 12;
        let word = be(body, at, 4)?;
        // Hierarchical indexes (reference_type 1) are not used by YouTube.
        if word & 0x8000_0000 != 0 { return None; }
        references.push((word & 0x7fff_ffff, be(body, at + 4, 4)?));
    }
    if timescale == 0 || references.is_empty() { return None; }
    Some(Sidx { timescale, first: end.checked_add(first_offset)?, references })
}

/// Inspect the first bytes of a file. `None` unless it starts with `ftyp`
/// and the complete `moov` box lies within `head`.
pub fn layout(head: &[u8]) -> Option<Layout> {
    let mut offset = 0usize;
    let (mut init_end, mut fragmented, mut sidx) = (None, false, None);
    while let Some((size, kind, header_len)) = header(head, offset) {
        if offset == 0 && &kind != b"ftyp" { return None; }
        let end = offset as u64 + size;
        match &kind {
            b"moov" => {
                let body = head.get(offset + header_len..usize::try_from(end).ok()?)?;
                let mut child = 0;
                while let Some((child_size, child_kind, _)) = header(body, child) {
                    fragmented |= &child_kind == b"mvex";
                    child = child.checked_add(usize::try_from(child_size).ok()?)?;
                }
                init_end = Some(end);
            }
            b"sidx" if sidx.is_none() => {
                sidx = head.get(offset + header_len..usize::try_from(end).ok()?)
                    .and_then(|body| parse_sidx(body, end));
            }
            b"moof" | b"mdat" => break,
            _ => {}
        }
        offset = usize::try_from(end).ok()?;
    }
    Some(Layout { init_end: init_end?, fragmented, sidx })
}

impl Sidx {
    /// Byte offset of the fragment containing `position` and that
    /// fragment's start time relative to the first fragment.
    pub fn fragment_at(&self, position: Duration) -> (u64, Duration) {
        let target = position.as_secs_f64() * f64::from(self.timescale);
        let (mut offset, mut ticks) = (self.first, 0u64);
        for (index, (size, duration)) in self.references.iter().enumerate() {
            if (ticks + duration) as f64 > target || index + 1 == self.references.len() { break; }
            offset += size;
            ticks += duration;
        }
        (offset, Duration::from_secs_f64(ticks as f64 / f64::from(self.timescale)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(body);
        out
    }

    fn sidx_v1(timescale: u32, first_offset: u64, refs: &[(u32, u32)]) -> Vec<u8> {
        let mut body = vec![1, 0, 0, 0, 0, 0, 0, 1];
        body.extend(timescale.to_be_bytes());
        body.extend(0u64.to_be_bytes());
        body.extend(first_offset.to_be_bytes());
        body.extend([0, 0]);
        body.extend((refs.len() as u16).to_be_bytes());
        for (size, duration) in refs {
            body.extend(size.to_be_bytes());
            body.extend(duration.to_be_bytes());
            body.extend(0x9000_0000u32.to_be_bytes());
        }
        mp4_box(b"sidx", &body)
    }

    /// Shape of a YouTube itag 140 / ffmpeg `-dash 1` file head.
    fn dash_head() -> Vec<u8> {
        let mut file = mp4_box(b"ftyp", b"dash\0\0\0\0iso6mp41");
        let mut moov = mp4_box(b"mvhd", &[0; 100]);
        moov.extend(mp4_box(b"trak", &[0; 40]));
        moov.extend(mp4_box(b"mvex", &mp4_box(b"trex", &[0; 24])));
        file.extend(mp4_box(b"moov", &moov));
        file.extend(sidx_v1(44100, 0, &[(1000, 441_000), (2000, 441_000), (500, 220_500)]));
        file.extend(mp4_box(b"moof", &[0; 16]));
        file
    }

    #[test]
    fn finds_init_and_sidx_of_fragmented_file() {
        let head = dash_head();
        let layout = layout(&head).unwrap();
        let moov_end = 24 + 8 + 108 + 48 + 8 + 32;
        assert_eq!(layout.init_end, moov_end);
        assert!(layout.fragmented);
        let sidx = layout.sidx.unwrap();
        assert_eq!(sidx.timescale, 44100);
        assert_eq!(sidx.first, moov_end + 8 + 32 + 3 * 12);
        assert_eq!(sidx.references, vec![(1000, 441_000), (2000, 441_000), (500, 220_500)]);
    }

    #[test]
    fn fragment_at_covers_boundaries() {
        let sidx = Sidx { timescale: 1000, first: 100, references: vec![(10, 10_000), (20, 10_000), (5, 5000)] };
        assert_eq!(sidx.fragment_at(Duration::ZERO), (100, Duration::ZERO));
        assert_eq!(sidx.fragment_at(Duration::from_millis(9_999)), (100, Duration::ZERO));
        assert_eq!(sidx.fragment_at(Duration::from_secs(10)), (110, Duration::from_secs(10)));
        assert_eq!(sidx.fragment_at(Duration::from_millis(22_500)), (130, Duration::from_secs(20)));
        assert_eq!(sidx.fragment_at(Duration::from_secs(99)), (130, Duration::from_secs(20)));
    }

    #[test]
    fn rejects_non_fragmented_or_truncated_heads() {
        let mut plain = mp4_box(b"ftyp", b"M4A \0\0\0\0");
        plain.extend(mp4_box(b"moov", &mp4_box(b"trak", &[0; 8])));
        let layout = layout(&plain).unwrap();
        assert!(!layout.fragmented && layout.sidx.is_none());
        let head = dash_head();
        assert!(super::layout(&head[..60]).is_none(), "moov cut off");
        assert!(super::layout(&mp4_box(b"moov", &[])).is_none(), "no leading ftyp");
        assert!(super::layout(b"ID3\x04\0\0\0\0\0\0").is_none(), "MP3");
        assert!(parse_sidx(&[1, 0, 0, 0], 0).is_none());
    }
}
