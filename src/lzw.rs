//! RE1 `.pak` LZW decoder.
//!
//! 9-bit initial codes packed MSB-first. `0x100` ends the stream, `0x101`
//! increases the code width by one and `0x102` resets the dictionary. The
//! first code after a start or reset is a raw literal byte.

use anyhow::{Result, bail};

const END: u16 = 0x100;
const WIDEN: u16 = 0x101;
const RESET: u16 = 0x102;
const FIRST_CODE: u16 = 0x103;
const DICT_ENTRIES: u16 = 34981;
const MAX_WIDTH: u8 = 16;

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read(&mut self, width: u8) -> Result<u16> {
        let mut value = 0u16;
        for _ in 0..width {
            let byte = self.bit >> 3;
            let Some(&b) = self.data.get(byte) else {
                bail!("truncated LZW stream");
            };
            value = (value << 1) | u16::from((b >> (7 - (self.bit & 7))) & 1);
            self.bit += 1;
        }
        Ok(value)
    }
}

struct Entry {
    prefix: u16,
    byte: u8,
}

/// Decode a RE1 `.pak` LZW stream.
pub fn decode(input: &[u8]) -> Result<Vec<u8>> {
    let mut reader = BitReader::new(input);
    let mut dict: Vec<Entry> = Vec::new();
    let mut next_code = FIRST_CODE;
    let mut width = 9u8;
    let mut prev: Option<u16> = None;
    let mut out = Vec::new();
    let mut string = Vec::new();

    loop {
        let code = reader.read(width)?;
        match code {
            END => return Ok(out),
            WIDEN => {
                if width >= MAX_WIDTH {
                    bail!("LZW code width exceeds {MAX_WIDTH}");
                }
                width += 1;
                continue;
            }
            RESET => {
                dict.clear();
                next_code = FIRST_CODE;
                width = 9;
                prev = None;
                continue;
            }
            _ => {}
        }

        let Some(prev_code) = prev else {
            if code > 0xff {
                bail!("LZW first code {code:#x} is not a literal");
            }
            out.push(code as u8);
            prev = Some(code);
            continue;
        };

        let special = next_code <= code;
        let lookup = if special { prev_code } else { code };

        string.clear();
        let mut c = lookup;
        while c >= FIRST_CODE {
            let Some(entry) = dict.get((c - FIRST_CODE) as usize) else {
                bail!("missing LZW dictionary entry {c:#x}");
            };
            string.push(entry.byte);
            c = entry.prefix;
        }
        string.push(c as u8);
        string.reverse();

        let first = string[0];
        if special {
            string.push(first);
        }
        out.extend_from_slice(&string);

        if next_code >= DICT_ENTRIES {
            bail!("LZW dictionary overflow");
        }
        dict.push(Entry {
            prefix: prev_code,
            byte: first,
        });
        next_code += 1;
        prev = Some(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct BitWriter {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn write(&mut self, code: u16, width: u8) {
            self.acc = (self.acc << width) | u32::from(code);
            self.nbits += u32::from(width);
            while self.nbits >= 8 {
                self.nbits -= 8;
                self.out.push(((self.acc >> self.nbits) & 0xff) as u8);
            }
            self.acc &= (1 << self.nbits) - 1;
        }

        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                self.out.push(((self.acc << (8 - self.nbits)) & 0xff) as u8);
            }
            self.out
        }
    }

    fn encode(data: &[u8]) -> Vec<u8> {
        let mut writer = BitWriter::new();
        let mut table: HashMap<Vec<u8>, u16> = HashMap::new();
        let mut next = FIRST_CODE;
        let mut width = 9u8;
        let mut cur: Vec<u8> = Vec::new();

        for &byte in data {
            let mut joined = cur.clone();
            joined.push(byte);
            if cur.is_empty() {
                cur = joined;
                continue;
            }
            if table.contains_key(&joined) {
                cur = joined;
                continue;
            }
            let code = if cur.len() > 1 {
                table[&cur]
            } else {
                u16::from(cur[0])
            };
            writer.write(code, width);
            if next >= DICT_ENTRIES {
                writer.write(RESET, width);
                table.clear();
                next = FIRST_CODE;
                width = 9;
            } else {
                table.insert(joined, next);
                next += 1;
                if u32::from(next) > (1u32 << width) - 1 && width < MAX_WIDTH {
                    writer.write(WIDEN, width);
                    width += 1;
                }
            }
            cur = vec![byte];
        }

        if !cur.is_empty() {
            let code = if cur.len() > 1 {
                table[&cur]
            } else {
                u16::from(cur[0])
            };
            writer.write(code, width);
        }
        writer.write(END, width);
        writer.finish()
    }

    fn roundtrip(data: &[u8]) {
        let encoded = encode(data);
        let decoded = decode(&encoded).expect("decode failed");
        if decoded != data {
            let at = decoded.iter().zip(data).position(|(a, b)| a != b);
            panic!(
                "mismatch at {at:?}: decoded {} bytes, expected {}",
                decoded.len(),
                data.len()
            );
        }
    }

    #[test]
    fn known_answer_literals() {
        let stream = [0x08, 0x00, 0x20, 0x00];
        assert_eq!(decode(&stream).unwrap(), vec![0x10, 0x00]);
    }

    #[test]
    fn roundtrip_short() {
        roundtrip(b"hello world");
    }

    #[test]
    fn roundtrip_empty() {
        roundtrip(b"");
    }

    #[test]
    fn roundtrip_kwkwk_runs() {
        roundtrip(&[0xaa; 4096]);
    }

    #[test]
    fn roundtrip_width_growth() {
        let mut state = 0x1234_5678u32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1103515245).wrapping_add(12345);
                (state >> 16) as u8
            })
            .collect();
        roundtrip(&data);
    }

    #[test]
    fn roundtrip_dictionary_reset() {
        let mut state = 0x0bad_c0deu32;
        let data: Vec<u8> = (0..40_000)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect();
        roundtrip(&data);
    }

    #[test]
    fn widen_control() {
        let mut writer = BitWriter::new();
        writer.write(0x41, 9);
        writer.write(WIDEN, 9);
        writer.write(0x42, 10);
        writer.write(END, 10);
        assert_eq!(decode(&writer.finish()).unwrap(), b"AB");
    }

    #[test]
    fn reset_control() {
        let mut writer = BitWriter::new();
        writer.write(0x41, 9);
        writer.write(RESET, 9);
        writer.write(0x42, 9);
        writer.write(END, 9);
        assert_eq!(decode(&writer.finish()).unwrap(), b"AB");
    }

    #[test]
    fn rejects_truncated_code() {
        assert!(decode(&[0x08]).is_err());
    }

    #[test]
    fn rejects_non_literal_first_code() {
        let mut writer = BitWriter::new();
        writer.write(FIRST_CODE, 9);
        assert!(decode(&writer.finish()).is_err());
    }

    #[test]
    fn out_of_order_code_is_treated_as_kwkwk() {
        let mut writer = BitWriter::new();
        writer.write(0x41, 9);
        writer.write(FIRST_CODE + 1, 9);
        writer.write(END, 9);
        assert_eq!(decode(&writer.finish()).unwrap(), b"AAA");
    }

    #[test]
    fn rejects_widen_at_max_width() {
        let mut writer = BitWriter::new();
        for width in 9..MAX_WIDTH {
            writer.write(WIDEN, width);
        }
        writer.write(WIDEN, MAX_WIDTH);
        assert!(decode(&writer.finish()).is_err());
    }

    #[test]
    fn rejects_dictionary_overflow() {
        let mut writer = BitWriter::new();
        let mut next = FIRST_CODE;
        let mut width = 9u8;
        writer.write(0x00, width);
        while next < DICT_ENTRIES {
            writer.write(0x00, width);
            next += 1;
            if u32::from(next) > (1u32 << width) - 1 && width < MAX_WIDTH {
                writer.write(WIDEN, width);
                width += 1;
            }
        }
        writer.write(0x00, width);
        writer.write(END, width);
        assert!(decode(&writer.finish()).is_err());
    }
}
