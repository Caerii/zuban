use std::sync::{Arc, OnceLock};

use anyhow::bail;
use parsa_python_cst::CodeIndex;
use regex::Regex;

use crate::InputPosition;

lazy_static::lazy_static! {
    static ref NEWLINES: Regex = Regex::new(r"\n|\r\n|\r").unwrap();
}

pub fn split_lines(code: &str) -> impl Iterator<Item = &str> {
    NEWLINES.split(code)
}

#[derive(Clone)]
pub(crate) struct NewlineIndices(OnceLock<Arc<[CodeIndex]>>);

impl NewlineIndices {
    pub fn new() -> Self {
        Self(OnceLock::new())
    }

    /// Reuse known line starts when a single edit changes bytes without
    /// changing any newline. The caller verifies that condition.
    pub fn shifted_after_edit(
        &self,
        old_code: &str,
        end: usize,
        removed_len: usize,
        inserted_len: usize,
    ) -> Option<Self> {
        let mut lines = self.lines(old_code).to_vec();
        for offset in lines.iter_mut().filter(|offset| **offset as usize > end) {
            let shifted = (*offset as usize)
                .checked_sub(removed_len)?
                .checked_add(inserted_len)?;
            *offset = shifted.try_into().ok()?;
        }
        Some(Self(OnceLock::from(Arc::from(lines))))
    }

    fn lines(&self, code: &str) -> &[CodeIndex] {
        self.0.get_or_init(|| {
            let mut v = vec![];
            let bytes = code.as_bytes();
            let mut index = 0;
            while index < bytes.len() {
                match bytes[index] {
                    b'\r' => {
                        index += 1;
                        if bytes.get(index) == Some(&b'\n') {
                            index += 1;
                        }
                        v.push(index as CodeIndex);
                    }
                    b'\n' => {
                        index += 1;
                        v.push(index as CodeIndex);
                    }
                    _ => index += 1,
                }
            }
            Arc::from(v)
        })
    }

    pub fn line_column_to_safe_byte(
        &self,
        code: &str,
        input: InputPosition,
    ) -> anyhow::Result<CodeIndex> {
        let pos = self.line_column_to_byte(code, input)?;
        if pos.column_out_of_bounds {
            bail!("Columns are out of bounds");
        }
        Ok(pos.byte)
    }

    pub fn line_column_to_byte(
        &self,
        code: &str,
        input: InputPosition,
    ) -> anyhow::Result<BytePositionInfos> {
        let line_infos = |line| {
            let lines = self.lines(code);
            let Some(next_line_start) = lines.get(line) else {
                if lines.len() == line {
                    return Ok(if lines.is_empty() {
                        (0, code)
                    } else {
                        let start = lines[line - 1];
                        (start, &code[start as usize..])
                    });
                }
                anyhow::bail!(
                    "File has only {} lines, but line {} is requested",
                    line + 1,
                    lines.len() + 1
                );
            };
            let start = if line == 0 { 0 } else { lines[line - 1] };
            let mut line_code = &code[start as usize..*next_line_start as usize - 1];
            if let Some(l) = line_code.strip_suffix('\r') {
                line_code = l
            }
            Ok((start, line_code))
        };

        Ok(match input {
            InputPosition::NthUTF8Byte(pos) => {
                let byte = pos.min(code.len());
                if !code.is_char_boundary(byte) {
                    anyhow::bail!("{pos} is not a valid char boundary");
                }
                BytePositionInfos {
                    byte: byte as CodeIndex,
                    column_out_of_bounds: byte < pos,
                }
            }
            InputPosition::Utf8Bytes { line, column } => {
                let (start, rest_line) = line_infos(line)?;
                let out_column = column.min(rest_line.len());

                if !rest_line.is_char_boundary(out_column) {
                    anyhow::bail!(
                        "Column {column} is not a valid char boundary on line {rest_line:?}"
                    );
                }
                BytePositionInfos {
                    byte: start + out_column as CodeIndex,
                    column_out_of_bounds: out_column < column,
                }
            }
            InputPosition::Utf16CodeUnits { line, column } => {
                let (start, rest_line) = line_infos(line)?;
                let mut infos = utf16_to_utf8_byte_offset(rest_line, column)?;
                infos.byte += start;
                infos
            }
            InputPosition::CodePoints { line, column } => {
                let (start, rest_line) = line_infos(line)?;
                let mut infos = codepoints_to_utf8_byte_offset(rest_line, column);
                infos.byte += start;
                infos
            }
        })
    }

    pub fn numbers_with_lines<'code>(
        &self,
        code: &'code str,
        skip_n_lines: usize,
    ) -> impl Iterator<Item = (usize, &'code str)> {
        let line_indexes = self.lines(code);
        let mut previous = 0;
        if skip_n_lines != 0 {
            previous = line_indexes
                .get(skip_n_lines - 1)
                .copied()
                .unwrap_or(line_indexes.len() as CodeIndex) as usize;
        }
        line_indexes
            .iter()
            .copied()
            .chain(if let Some(last) = line_indexes.last() {
                (*last as usize != code.len()).then(|| code.len() as CodeIndex + 1)
            } else {
                Some(code.len() as CodeIndex + 1)
            })
            .enumerate()
            .skip(skip_n_lines)
            .map(move |(line_nr, line_index)| {
                let line_index = line_index as usize;
                let Some(mut result) = code.get(previous..line_index - 1) else {
                    panic!("Wanted to access {previous}..{line_index} for code {code:?}");
                };
                if let Some(l) = result.strip_suffix('\r') {
                    result = l
                }
                previous = line_index;
                (line_nr, result)
            })
    }

    pub fn position_infos<'code>(
        &self,
        code: &'code str,
        byte_position: CodeIndex,
    ) -> PositionInfos<'code> {
        let lines = self.lines(code);
        let line = lines.partition_point(|&l| l <= byte_position);
        PositionInfos {
            line,
            code,
            line_offset_in_code: line
                .checked_sub(1)
                .map(|line| lines[line] as usize)
                .unwrap_or(0),
            byte_position: byte_position as usize,
        }
    }

    pub fn expand_range_to_full_lines(
        &self,
        code: &str,
        range: std::ops::Range<CodeIndex>,
    ) -> std::ops::Range<CodeIndex> {
        let infos_start = self.position_infos(code, range.start);
        let infos_end = self.position_infos(code, range.end);
        let lines = self.lines(code);
        let end = if let Some(end) = lines.get(infos_end.line) {
            *end - 1
        } else {
            code.len() as CodeIndex
        };
        infos_start.line_offset_in_code as CodeIndex..end
    }
}

fn ascii_prefix_byte_offset(s: &str, column: usize) -> Option<CodeIndex> {
    s.as_bytes()
        .get(..column)
        .filter(|prefix| prefix.is_ascii())
        .map(|_| column as CodeIndex)
}

fn codepoints_to_utf8_byte_offset(s: &str, column: usize) -> BytePositionInfos {
    if let Some(byte) = ascii_prefix_byte_offset(s, column) {
        return BytePositionInfos {
            byte,
            column_out_of_bounds: false,
        };
    }
    let mut byte = 0;
    let mut count = 0;
    for c in s.chars().take(column) {
        byte += c.len_utf8() as CodeIndex;
        count += 1;
    }
    BytePositionInfos {
        byte,
        column_out_of_bounds: count < column,
    }
}

fn utf16_to_utf8_byte_offset(s: &str, utf16_pos: usize) -> anyhow::Result<BytePositionInfos> {
    if let Some(byte) = ascii_prefix_byte_offset(s, utf16_pos) {
        return Ok(BytePositionInfos {
            byte,
            column_out_of_bounds: false,
        });
    }
    let mut utf16_count = 0;

    for (utf8_idx, c) in s.char_indices() {
        if utf16_count == utf16_pos {
            return Ok(BytePositionInfos {
                byte: utf8_idx as CodeIndex,
                column_out_of_bounds: false,
            });
        }

        let char_utf16_len = c.len_utf16();
        if utf16_count + char_utf16_len > utf16_pos {
            // Position is in the middle of this char -> invalid
            anyhow::bail!("Column {utf16_pos} is not a valid code unit boundary on line {s:?}",);
        }

        utf16_count += char_utf16_len;
    }
    Ok(BytePositionInfos {
        byte: s.len() as CodeIndex,
        column_out_of_bounds: utf16_pos > utf16_count,
    })
}

#[derive(Copy, Clone)]
pub(crate) struct BytePositionInfos {
    pub byte: CodeIndex,
    pub column_out_of_bounds: bool,
}

#[derive(Copy, Clone, Debug)]
pub struct PositionInfos<'code> {
    line: usize, // zero-based line number
    pub(crate) line_offset_in_code: usize,
    code: &'code str,
    pub byte_position: usize,
}

impl<'code> PositionInfos<'code> {
    // All columns are zero-based
    pub fn utf8_bytes_column(&self) -> usize {
        self.byte_position - self.line_offset_in_code
    }

    pub fn utf16_code_units_column(&self) -> usize {
        self.line_part().encode_utf16().count()
    }

    pub fn code_points_column(&self) -> usize {
        self.line_part().chars().count()
    }

    pub fn line_zero_based(&self) -> usize {
        self.line
    }

    pub fn line_one_based(&self) -> usize {
        self.line + 1
    }

    fn line_part(&self) -> &str {
        &self.code[self.line_offset_in_code..self.byte_position]
    }

    pub(crate) fn code_until(&self, end_pos: PositionInfos) -> &'code str {
        &self.code[self.byte_position..end_pos.byte_position]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shifted_line_indices_match_fresh_scan() {
        for (old, end, removed, inserted, new) in [
            ("a\nb\nc\n", 1, 0, 2, "aXX\nb\nc\n"),
            ("abc\nb\nc\n", 3, 2, 0, "a\nb\nc\n"),
            ("a\n😀b\nc\n", 6, 0, 1, "a\n😀Xb\nc\n"),
            ("a\r\nb\r\nc\r\n", 4, 0, 2, "a\r\nbYY\r\nc\r\n"),
        ] {
            let shifted = NewlineIndices::new()
                .shifted_after_edit(old, end, removed, inserted)
                .unwrap();
            assert_eq!(shifted.lines(new), NewlineIndices::new().lines(new));
        }
    }

    #[test]
    fn test_shifted_line_indices_for_non_newline_edits() {
        for old in ["a\nb\n", "a\r\nb\r\n", "é\n😀x\n", "a\rb\n"] {
            let old_indices = NewlineIndices::new();
            for start in 0..=old.len() {
                if !old.is_char_boundary(start) {
                    continue;
                }
                for end in start..=old.len() {
                    if !old.is_char_boundary(end) || old[start..end].contains(['\r', '\n']) {
                        continue;
                    }
                    for inserted in ["", "x", "é"] {
                        if old.as_bytes().get(start.wrapping_sub(1)) == Some(&b'\r')
                            && old.as_bytes().get(end) == Some(&b'\n')
                            && (start == end || inserted.is_empty())
                        {
                            continue;
                        }
                        let new = format!("{}{}{}", &old[..start], inserted, &old[end..]);
                        let shifted = old_indices
                            .shifted_after_edit(old, end, end - start, inserted.len())
                            .unwrap();
                        assert_eq!(
                            shifted.lines(&new),
                            NewlineIndices::new().lines(&new),
                            "old={old:?} start={start} end={end} inserted={inserted:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_newline_indices_no_line() {
        let indices = NewlineIndices::new();
        let code = "ä";
        assert!(indices.lines(code).is_empty());
        assert_eq!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 0, column: 2 })
                .unwrap()
                .byte,
            2
        );
        assert_eq!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 0, column: 3 })
                .unwrap()
                .byte,
            2
        );
        assert!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 0, column: 1 })
                .is_err()
        );
    }

    #[test]
    fn test_newline_indices_mixed_endings() {
        let indices = NewlineIndices::new();
        assert_eq!(indices.lines("a\r\nb\rc\né\r\n\n"), [3, 5, 7, 11, 12]);
        let indices = NewlineIndices::new();
        assert_eq!(indices.lines("\r\r\n\n"), [1, 3, 4]);
    }

    #[test]
    fn test_crlf_columns_exclude_carriage_return() {
        let code = "a\r\nb\n";
        let indices = NewlineIndices::new();
        for input in [
            InputPosition::Utf8Bytes { line: 0, column: 1 },
            InputPosition::Utf16CodeUnits { line: 0, column: 1 },
            InputPosition::CodePoints { line: 0, column: 1 },
        ] {
            assert_eq!(indices.line_column_to_safe_byte(code, input).unwrap(), 1);
        }
        for input in [
            InputPosition::Utf8Bytes { line: 0, column: 2 },
            InputPosition::Utf16CodeUnits { line: 0, column: 2 },
            InputPosition::CodePoints { line: 0, column: 2 },
        ] {
            assert!(indices.line_column_to_safe_byte(code, input).is_err());
        }
        assert_eq!(
            indices.numbers_with_lines(code, 0).collect::<Vec<_>>(),
            [(0, "a"), (1, "b")]
        );
    }

    #[test]
    fn test_codepoint_and_utf16_offsets_across_unicode() {
        let code = "a😀éz";
        let indices = NewlineIndices::new();
        for (column, byte) in [(0, 0), (1, 1), (2, 5), (3, 7), (4, 8)] {
            assert_eq!(
                indices
                    .line_column_to_safe_byte(code, InputPosition::CodePoints { line: 0, column })
                    .unwrap(),
                byte
            );
        }
        for (column, byte) in [(0, 0), (1, 1), (3, 5), (4, 7), (5, 8)] {
            assert_eq!(
                indices
                    .line_column_to_safe_byte(
                        code,
                        InputPosition::Utf16CodeUnits { line: 0, column }
                    )
                    .unwrap(),
                byte
            );
        }
        assert!(
            indices
                .line_column_to_safe_byte(code, InputPosition::CodePoints { line: 0, column: 5 })
                .is_err()
        );
        assert!(
            indices
                .line_column_to_safe_byte(
                    code,
                    InputPosition::Utf16CodeUnits { line: 0, column: 2 }
                )
                .is_err()
        );
    }

    #[test]
    fn test_codepoint_offsets_with_non_ascii_first_character() {
        let code = "😀x";
        let indices = NewlineIndices::new();
        for (column, byte) in [(0, 0), (1, 4), (2, 5)] {
            assert_eq!(
                indices
                    .line_column_to_safe_byte(code, InputPosition::CodePoints { line: 0, column })
                    .unwrap(),
                byte
            );
        }
        assert!(
            indices
                .line_column_to_safe_byte(code, InputPosition::CodePoints { line: 0, column: 3 })
                .is_err()
        );
    }

    #[test]
    fn test_utf16_position_after_non_bmp_character() {
        let code = "😀";
        let indices = NewlineIndices::new();
        assert_eq!(
            indices
                .line_column_to_safe_byte(
                    code,
                    InputPosition::Utf16CodeUnits { line: 0, column: 2 }
                )
                .unwrap(),
            code.len() as CodeIndex
        );
        assert!(
            indices
                .line_column_to_safe_byte(
                    code,
                    InputPosition::Utf16CodeUnits { line: 0, column: 1 }
                )
                .is_err()
        );
        assert!(
            indices
                .line_column_to_safe_byte(
                    code,
                    InputPosition::Utf16CodeUnits { line: 0, column: 3 }
                )
                .is_err()
        );
    }

    #[test]
    fn test_newline_indices_without_ending_newlines() {
        let indices = NewlineIndices::new();
        let code = "x\nä";
        assert_eq!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 1, column: 2 })
                .unwrap()
                .byte,
            4
        );
        assert_eq!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 1, column: 3 })
                .unwrap()
                .byte,
            4
        );
        assert!(
            indices
                .line_column_to_byte(code, InputPosition::Utf8Bytes { line: 1, column: 1 })
                .is_err()
        );
    }

    #[test]
    fn test_numbers_with_lines() {
        let check = |code, skip_lines| {
            let indices = NewlineIndices::new();
            indices
                .numbers_with_lines(code, skip_lines)
                .collect::<Vec<_>>()
        };
        let c1 = "x\nä\ny";
        assert_eq!(check(c1, 0), [(0, "x"), (1, "ä"), (2, "y")]);
        assert_eq!(check(c1, 1), [(1, "ä"), (2, "y")]);
        assert_eq!(check(c1, 2), [(2, "y")]);
        assert_eq!(check(c1, 3), []);
        assert_eq!(check(c1, 4), []);

        let c2 = "x\ny\n";
        assert_eq!(check(c2, 0), [(0, "x"), (1, "y")]);
        assert_eq!(check(c2, 1), [(1, "y")]);
        assert_eq!(check(c2, 2), []);
        assert_eq!(check(c2, 3), []);

        let c3 = "x\n";
        assert_eq!(check(c3, 0), [(0, "x")]);
        assert_eq!(check(c3, 1), []);
        assert_eq!(check(c3, 2), []);

        let c4 = "x";
        //assert_eq!(check(c4, 0), [(0, "x")]);
        assert_eq!(check(c4, 1), []);
        assert_eq!(check(c4, 2), []);

        let c5 = "";
        assert_eq!(check(c5, 0), [(0, "")]);
        assert_eq!(check(c5, 1), []);
        assert_eq!(check(c5, 2), []);
    }

    #[test]
    fn test_expand_range_to_full_lines() {
        let check = |code, skip_lines| {
            let indices = NewlineIndices::new();
            indices.expand_range_to_full_lines(code, skip_lines)
        };
        let c1 = "";
        assert_eq!(check(c1, 0..0), 0..0);
        assert_eq!(check(c1, 0..1), 0..0);

        let c2 = "x";
        assert_eq!(check(c2, 0..0), 0..1);
        assert_eq!(check(c2, 0..1), 0..1);
        assert_eq!(check(c2, 0..2), 0..1);
        assert_eq!(check(c2, 1..1), 0..1);
        assert_eq!(check(c2, 1..5), 0..1);

        let c2 = "x\ny\nz";
        assert_eq!(check(c2, 0..0), 0..1);
        assert_eq!(check(c2, 0..1), 0..1);
        assert_eq!(check(c2, 0..2), 0..3);
        assert_eq!(check(c2, 1..1), 0..1);
        assert_eq!(check(c2, 1..3), 0..3);
        assert_eq!(check(c2, 2..2), 2..3);
        assert_eq!(check(c2, 2..3), 2..3);
        assert_eq!(check(c2, 3..3), 2..3);
        assert_eq!(check(c2, 3..4), 2..5);
        assert_eq!(check(c2, 4..4), 4..5);
        assert_eq!(check(c2, 1..5), 0..5);
        assert_eq!(check(c2, 1..8), 0..5);
    }
}
