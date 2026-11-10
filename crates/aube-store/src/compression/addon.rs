//! `addon` feature — unwrap a napi `--compress` hybrid back to the raw `.node`.
//!
//! A binpress hybrid carries the original addon, zstd-compressed, inside a
//! `PRESSED_DATA` SIGNABLE SECTION (Mach-O `__PRESSED_DATA` in segment `SMOL`, ELF
//! `.PRESSED_DATA`, PE `.PRESSED` — read from the binary's SECTION HEADERS, NOT an
//! EOF footer). The section content is the bin-infra pressed-data blob:
//!
//! ```text
//! [magic marker  32B]  "__SMOL_PRESSED_DATA_MAGIC_MARKER"
//! [compressed    u64 LE]  zstd payload length
//! [uncompressed  u64 LE]  raw addon length
//! [cache key     16B]  hex, ignored here
//! [platform      3B ]  platform/arch/libc, ignored here
//! [integrity     64B]  SHA-512 of the zstd payload
//! [has_config    1B ]  0/1
//! [config        1192B] only if has_config == 1
//! [payload       compressed bytes]  zstd frame
//! ```
//!
//! `unwrap_if_hybrid(&content)` returns `Some(raw addon)` for a hybrid whose payload
//! zstd-decodes to `uncompressed` bytes and matches the SHA-512, else `None`. The
//! PM helper then feeds the raw addon to `compress_bytes`. This is the
//! mirror-image ABI of `socket-btm/packages/bin-infra` (`smol_segment_reader.c` /
//! `compression_constants.h`).

use std::io::Read;

use sha2::{Digest, Sha512};

/// "__SMOL_PRESSED_DATA_MAGIC_MARKER" (SMOL_PRESSED_DATA_MAGIC_MARKER).
const MAGIC_MARKER: &[u8; 32] = b"__SMOL_PRESSED_DATA_MAGIC_MARKER";
const SIZE_HEADER_LEN: usize = 16; // compressed u64 + uncompressed u64
const CACHE_KEY_LEN: usize = 16;
const PLATFORM_METADATA_LEN: usize = 3;
const INTEGRITY_HASH_LEN: usize = 64; // SHA-512
const SMOL_CONFIG_FLAG_LEN: usize = 1;
const SMOL_CONFIG_BINARY_LEN: usize = 1192;
/// Fixed header length up to and including the has-config flag.
const HEADER_LEN: usize = MAGIC_MARKER.len()
    + SIZE_HEADER_LEN
    + CACHE_KEY_LEN
    + PLATFORM_METADATA_LEN
    + INTEGRITY_HASH_LEN
    + SMOL_CONFIG_FLAG_LEN;
/// Refuse a decompressed-size claim past this (DoS guard, matches bin-infra's 512 MB).
const MAX_DECOMPRESSED: u64 = 512 * 1024 * 1024;
/// Small addon frames benefit from zstd's bulk decoder and a single exact-size
/// output allocation. Larger payloads use the streaming decoder to keep peak
/// working memory bounded while they are decoded.
const BULK_DECODE_LIMIT: u64 = 64 * 1024 * 1024;

/// If `content` is a napi `--compress` hybrid, decode its embedded addon and return
/// the raw `.node` bytes; otherwise `None`. Integrity-checked (SHA-512 over the zstd
/// payload) — a hybrid that fails verification returns `None`, never partial bytes.
pub fn unwrap_if_hybrid(content: &[u8]) -> Option<Vec<u8>> {
    let section = find_pressed_data_section(content)?;
    decode_pressed_data(section)
}

/// Parse the bin-infra pressed-data blob (magic + header + zstd payload) into the
/// raw addon. Split from section-finding so the format round-trips in a unit test
/// without synthesizing a whole Mach-O/ELF/PE.
pub fn decode_pressed_data(section: &[u8]) -> Option<Vec<u8>> {
    if section.len() < HEADER_LEN {
        return None;
    }
    if &section[..MAGIC_MARKER.len()] != MAGIC_MARKER.as_slice() {
        return None;
    }
    let mut at = MAGIC_MARKER.len();
    let compressed_size = read_u64_le(section, at)?;
    at += 8;
    let uncompressed_size = read_u64_le(section, at)?;
    at += 8;
    // Skip cache key + platform metadata.
    at += CACHE_KEY_LEN + PLATFORM_METADATA_LEN;
    let integrity = section.get(at..at + INTEGRITY_HASH_LEN)?;
    let mut hash = [0u8; INTEGRITY_HASH_LEN];
    hash.copy_from_slice(integrity);
    at += INTEGRITY_HASH_LEN;
    let has_config = *section.get(at)?;
    at += SMOL_CONFIG_FLAG_LEN;
    if has_config != 0 {
        at = at.checked_add(SMOL_CONFIG_BINARY_LEN)?;
    }

    if compressed_size == 0
        || uncompressed_size == 0
        || uncompressed_size > MAX_DECOMPRESSED
        || compressed_size > MAX_DECOMPRESSED
    {
        return None;
    }
    let payload = section.get(at..at.checked_add(compressed_size as usize)?)?;

    // Integrity: SHA-512 of the zstd payload, BEFORE decompressing (reject a
    // tampered frame up front).
    let mut hasher = Sha512::new();
    hasher.update(payload);
    if hasher.finalize().as_slice() != hash {
        return None;
    }

    decode_zstd_payload(payload, uncompressed_size)
}

/// Decode one native-addon frame with the production bulk-allocation threshold.
fn decode_zstd_payload(payload: &[u8], expected_size: u64) -> Option<Vec<u8>> {
    decode_zstd_payload_with_bulk_limit(payload, expected_size, BULK_DECODE_LIMIT)
}

/// Reject trailing frames and size mismatches while bounding output and decoder history.
fn decode_zstd_payload_with_bulk_limit(
    payload: &[u8],
    expected_size: u64,
    bulk_decode_limit: u64,
) -> Option<Vec<u8>> {
    let expected = usize::try_from(expected_size).ok()?;
    // The embedded format carries exactly one frame. Reject concatenated frames
    // and trailing bytes before decoding, so neither path can silently accept
    // extra output beyond the header's claimed size.
    let frame_size = zstd::zstd_safe::find_frame_compressed_size(payload).ok()?;
    if frame_size != payload.len() {
        return None;
    }

    // Current napi-rs frames may retain a 2 MiB default window even for small
    // payloads. Keep that compatibility floor, then scale the limit with the
    // claimed output for large native addons.
    let window_log = (64 - expected_size.saturating_sub(1).leading_zeros()).max(21);
    let raw = if expected_size <= bulk_decode_limit {
        let mut decoder = zstd::bulk::Decompressor::new().ok()?;
        decoder
            .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(window_log))
            .ok()?;
        decoder.decompress(payload, expected).ok()?
    } else {
        let mut context = zstd::zstd_safe::DCtx::create();
        context
            .set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(window_log))
            .ok()?;
        let decoder =
            zstd::stream::read::Decoder::with_context(std::io::Cursor::new(payload), &mut context)
                .single_frame();
        // Read at most one byte past the declared output size. This rejects a
        // dishonest size header without allowing an unbounded decode_all Vec.
        let mut limited = decoder.take(expected_size.checked_add(1)?);
        let mut raw = Vec::new();
        raw.try_reserve_exact(expected).ok()?;
        limited.read_to_end(&mut raw).ok()?;
        raw
    };
    (raw.len() == expected).then_some(raw)
}

/// Read a little-endian integer; reject truncated or overflowing offsets.
fn read_u64_le(buf: &[u8], at: usize) -> Option<u64> {
    let bytes = buf.get(at..at.checked_add(8)?)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(bytes);
    Some(u64::from_le_bytes(arr))
}

/// Read a little-endian integer; reject truncated or overflowing offsets.
fn read_u32_le(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    let mut arr = [0u8; 4];
    arr.copy_from_slice(bytes);
    Some(u32::from_le_bytes(arr))
}

/// Read a little-endian integer; reject truncated or overflowing offsets.
fn read_u16_le(buf: &[u8], at: usize) -> Option<u16> {
    let bytes = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Locate the PRESSED_DATA section's raw bytes by walking the binary's section /
/// load-command table — never an EOF footer. Dispatches on the leading magic.
fn find_pressed_data_section(content: &[u8]) -> Option<&[u8]> {
    match content.get(..4)? {
        // Mach-O 64-bit, both endiannesses.
        [0xcf, 0xfa, 0xed, 0xfe] | [0xfe, 0xed, 0xfa, 0xcf] => find_macho(content),
        [0x7f, b'E', b'L', b'F'] => find_elf(content),
        [b'M', b'Z', ..] => find_pe(content),
        _ => None,
    }
}

/// Mach-O 64-bit (little-endian host): find segment `SMOL` → section `__PRESSED_DATA`
/// → its (offset,size) and return the slice. Mirrors smol_segment_reader.c.
fn find_macho(content: &[u8]) -> Option<&[u8]> {
    const LC_SEGMENT_64: u32 = 0x19;
    // mach_header_64: magic(4) cputype(4) cpusubtype(4) filetype(4) ncmds(4) ...
    let ncmds = read_u32_le(content, 16)?;
    let mut cmd_off = 32usize; // sizeof(mach_header_64)
    for _ in 0..ncmds.min(10_000) {
        let cmd = read_u32_le(content, cmd_off)?;
        let cmdsize = read_u32_le(content, cmd_off.checked_add(4)?)? as usize;
        if cmdsize == 0 {
            return None;
        }
        if cmd == LC_SEGMENT_64 {
            // segment_command_64: cmd(4) cmdsize(4) segname(16) vmaddr(8) vmsize(8)
            //   fileoff(8) filesize(8) maxprot(4) initprot(4) nsects(4) flags(4)
            let segname = content.get(cmd_off.checked_add(8)?..cmd_off.checked_add(24)?)?;
            if name_eq(segname, b"SMOL") {
                let nsects = read_u32_le(content, cmd_off.checked_add(64)?)?;
                let mut sect_off = cmd_off.checked_add(72)?; // start of section_64 array
                for _ in 0..nsects.min(1000) {
                    // section_64: sectname(16) segname(16) addr(8) size(8) offset(4) ...
                    let sectname = content.get(sect_off..sect_off.checked_add(16)?)?;
                    if name_eq(sectname, b"__PRESSED_DATA") {
                        let size = read_u64_le(content, sect_off.checked_add(40)?)? as usize;
                        let offset = read_u32_le(content, sect_off.checked_add(48)?)? as usize;
                        return content.get(offset..offset.checked_add(size)?);
                    }
                    sect_off = sect_off.checked_add(80)?; // sizeof(section_64)
                }
            }
        }
        cmd_off = cmd_off.checked_add(cmdsize)?;
    }
    None
}

/// ELF 64-bit: walk the section header table, match `.PRESSED_DATA` against the
/// section-header string table, return its (sh_offset, sh_size) slice.
fn find_elf(content: &[u8]) -> Option<&[u8]> {
    // EI_CLASS at offset 4: 2 == 64-bit. Only 64-bit addons ship.
    if *content.get(4)? != 2 {
        return None;
    }
    let e_shoff = read_u64_le(content, 40)? as usize;
    let e_shentsize = read_u16_le(content, 58)? as usize;
    let e_shnum = read_u16_le(content, 60)? as usize;
    let e_shstrndx = read_u16_le(content, 62)? as usize;
    if e_shentsize < 64 || e_shnum == 0 || e_shstrndx >= e_shnum {
        return None;
    }
    // String table section header → its (offset,size).
    let strtab_hdr = e_shoff.checked_add(e_shstrndx.checked_mul(e_shentsize)?)?;
    let strtab_off = read_u64_le(content, strtab_hdr.checked_add(24)?)? as usize;
    let strtab_size = read_u64_le(content, strtab_hdr.checked_add(32)?)? as usize;
    let strtab = content.get(strtab_off..strtab_off.checked_add(strtab_size)?)?;

    for i in 0..e_shnum {
        let shdr = e_shoff.checked_add(i.checked_mul(e_shentsize)?)?;
        let sh_name = read_u32_le(content, shdr)? as usize;
        if cstr_at(strtab, sh_name) == Some(b".PRESSED_DATA".as_slice()) {
            let sh_offset = read_u64_le(content, shdr.checked_add(24)?)? as usize;
            let sh_size = read_u64_le(content, shdr.checked_add(32)?)? as usize;
            return content.get(sh_offset..sh_offset.checked_add(sh_size)?);
        }
    }
    None
}

/// PE: parse the section table for `.PRESSED` (the 8-byte-name truncation of
/// `.PRESSED_DATA`) and return (PointerToRawData, SizeOfRawData).
fn find_pe(content: &[u8]) -> Option<&[u8]> {
    let pe_off = read_u32_le(content, 0x3c)? as usize;
    if content.get(pe_off..pe_off.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let coff = pe_off.checked_add(4)?;
    let number_of_sections = read_u16_le(content, coff.checked_add(2)?)? as usize;
    let size_of_optional = read_u16_le(content, coff.checked_add(16)?)? as usize;
    if number_of_sections > 200 {
        return None;
    }
    let mut sect = coff.checked_add(20)?.checked_add(size_of_optional)?; // section table start
    for _ in 0..number_of_sections {
        let name = content.get(sect..sect.checked_add(8)?)?;
        if name == b".PRESSED" {
            let size_of_raw = read_u32_le(content, sect.checked_add(16)?)? as usize;
            let ptr_raw = read_u32_le(content, sect.checked_add(20)?)? as usize;
            return content.get(ptr_raw..ptr_raw.checked_add(size_of_raw)?);
        }
        sect = sect.checked_add(40)?; // sizeof(IMAGE_SECTION_HEADER)
    }
    None
}

/// Compare a fixed-width, NUL-padded name field against a logical name.
fn name_eq(field: &[u8], want: &[u8]) -> bool {
    if want.len() > field.len() {
        return false;
    }
    field[..want.len()] == *want && field[want.len()..].iter().all(|&b| b == 0)
}

/// The NUL-terminated string at `off` within a string table.
fn cstr_at(strtab: &[u8], off: usize) -> Option<&[u8]> {
    let rest = strtab.get(off..)?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[cfg(test)]
mod decode_tests {
    #[test]
    fn malformed_offsets_return_none_without_panicking() {
        assert!(super::read_u64_le(&[], usize::MAX).is_none());
        assert!(super::read_u32_le(&[], usize::MAX).is_none());
        assert!(super::read_u16_le(&[], usize::MAX).is_none());
        let mut elf = vec![0; 64];
        elf[..5].copy_from_slice(b"\x7fELF\x02");
        elf[40..48].copy_from_slice(&(usize::MAX as u64 - 8).to_le_bytes());
        elf[58..60].copy_from_slice(&64u16.to_le_bytes());
        elf[60..62].copy_from_slice(&1u16.to_le_bytes());
        assert!(super::find_elf(&elf).is_none());
    }

    use super::{decode_zstd_payload, decode_zstd_payload_with_bulk_limit};

    fn frame(raw: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(raw, 3).expect("encode fixture")
    }

    #[test]
    fn bulk_decode_matches_the_claimed_size() {
        let raw = b"native addon fixture";
        let decoded = decode_zstd_payload(&frame(raw), raw.len() as u64).unwrap();
        assert_eq!(decoded, raw);
        assert!(decode_zstd_payload(&frame(raw), (raw.len() - 1) as u64).is_none());
    }

    #[test]
    fn bounded_stream_decode_matches_the_claimed_size() {
        let raw = b"native addon fixture decoded through the streaming path";
        let compressed = frame(raw);
        let decoded =
            decode_zstd_payload_with_bulk_limit(&compressed, raw.len() as u64, 1).unwrap();
        assert_eq!(decoded, raw);
        assert!(
            decode_zstd_payload_with_bulk_limit(&compressed, (raw.len() - 1) as u64, 1).is_none()
        );
    }

    #[test]
    fn rejects_trailing_frame_bytes() {
        let raw = b"native addon fixture";
        let mut compressed = frame(raw);
        compressed.push(0);
        assert!(decode_zstd_payload(&compressed, raw.len() as u64).is_none());
    }

    #[test]
    #[ignore = "local decode comparison; run explicitly with AUBE_CODEC_BENCH_INPUTS"]
    fn compare_local_addon_decoders() {
        let inputs = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input paths");
        for path in std::env::split_paths(&inputs) {
            let raw = std::fs::read(&path).expect("read input");
            let payload = zstd::stream::encode_all(raw.as_slice(), 3).expect("compress fixture");
            println!(
                "{} raw_mb={:.1} zstd_mb={:.1}",
                path.display(),
                raw.len() as f64 / 1_000_000.0,
                payload.len() as f64 / 1_000_000.0
            );
            let mut timings: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
            for _ in 0..2 {
                let _ = zstd::stream::decode_all(payload.as_slice()).expect("warm legacy decode");
                let _ =
                    decode_zstd_payload(&payload, raw.len() as u64).expect("warm bounded decode");
            }
            for round in 0..9 {
                let order = if round % 2 == 0 { [0, 1] } else { [1, 0] };
                for decoder in order {
                    let start = std::time::Instant::now();
                    let decoded = if decoder == 0 {
                        zstd::stream::decode_all(payload.as_slice()).expect("legacy decode")
                    } else {
                        decode_zstd_payload(&payload, raw.len() as u64).expect("bounded decode")
                    };
                    timings[decoder].push(start.elapsed().as_secs_f64());
                    assert_eq!(decoded, raw);
                    std::hint::black_box(decoded);
                }
            }
            for (index, label) in ["decode_all", "bounded"].into_iter().enumerate() {
                timings[index].sort_by(f64::total_cmp);
                println!("decoder={label} median_s={:.4}", timings[index][4]);
            }
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[ignore = "local decoder peak-memory comparison; run explicitly with benchmark environment"]
    fn measure_local_decoder_peak_memory() {
        let input = std::env::var_os("AUBE_CODEC_BENCH_INPUTS").expect("input path");
        let input = std::env::split_paths(&input)
            .next()
            .expect("first input path");
        let raw = std::fs::read(input).expect("read input");
        let payload = zstd::stream::encode_all(raw.as_slice(), 3).expect("compress fixture");
        let mode = std::env::var("AUBE_ZSTD_BENCH_MODE").unwrap_or_else(|_| "bounded".into());
        let mut before = unsafe { std::mem::zeroed::<libc::rusage>() };
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut before) },
            0
        );
        let decoded = if mode == "legacy" {
            zstd::stream::decode_all(payload.as_slice()).expect("legacy decode")
        } else {
            decode_zstd_payload(&payload, raw.len() as u64).expect("bounded decode")
        };
        assert_eq!(decoded, raw);
        let mut after = unsafe { std::mem::zeroed::<libc::rusage>() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut after) }, 0);
        // getrusage reports peak RSS in KiB on Linux and bytes on macOS.
        // Convert both to decimal MB before displaying one decimal place.
        #[cfg(target_os = "linux")]
        const RSS_UNIT_BYTES: f64 = 1024.0;
        #[cfg(target_os = "macos")]
        const RSS_UNIT_BYTES: f64 = 1.0;
        println!(
            "mode={mode} raw_mb={:.1} compressed_mb={:.1} before_peak_mb={:.1} after_peak_mb={:.1}",
            raw.len() as f64 / 1_000_000.0,
            payload.len() as f64 / 1_000_000.0,
            before.ru_maxrss as f64 * RSS_UNIT_BYTES / 1_000_000.0,
            after.ru_maxrss as f64 * RSS_UNIT_BYTES / 1_000_000.0
        );
        std::hint::black_box(decoded);
    }
}
