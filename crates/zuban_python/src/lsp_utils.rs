use std::borrow::Cow;

use anyhow::bail;
use lsp_types::{Position, TextDocumentContentChangeEvent};

use crate::{InputPosition, lines::NewlineIndices};

pub(crate) fn apply_document_changes(
    file_contents: &str,
    line_indices: &NewlineIndices,
    mut content_changes: Vec<TextDocumentContentChangeEvent>,
    to_input_position: impl Fn(Position) -> InputPosition,
) -> anyhow::Result<String> {
    // A single incremental edit can construct the new document in one pass.
    // Copying the old text and then replacing a range moves its suffix twice.
    if let [change] = content_changes.as_slice()
        && let Some(range) = change.range
    {
        if range.start > range.end {
            bail!("Invalid range of a change, start is bigger than end: {range:?}");
        }
        let start = line_indices
            .line_column_to_safe_byte(file_contents, to_input_position(range.start))?
            as usize;
        let end = line_indices
            .line_column_to_safe_byte(file_contents, to_input_position(range.end))?
            as usize;
        let mut text =
            String::with_capacity(file_contents.len() - (end - start) + change.text.len());
        text.push_str(&file_contents[..start]);
        text.push_str(&change.text);
        text.push_str(&file_contents[end..]);
        return Ok(text);
    }

    // Small buffers and same-length edits are cheaper to update in place.
    // For larger, length-changing reverse batches, avoid moving a long suffix
    // once per edit. The position-width test is conservative for non-ASCII text.
    if file_contents.len() >= 8192
        && content_changes.len() > 1
        && content_changes.iter().any(|change| {
            change.range.is_some_and(|range| {
                range.start.line != range.end.line
                    || change.text.len()
                        != range.end.character.saturating_sub(range.start.character) as usize
            })
        })
    {
        let mut edits = Vec::with_capacity(content_changes.len());
        let mut previous_start = file_contents.len();
        let mut result_len = file_contents.len();
        for change in &content_changes {
            let Some(range) = change.range else {
                break;
            };
            if range.start > range.end {
                break;
            }
            let (Ok(start), Ok(end)) = (
                line_indices
                    .line_column_to_safe_byte(file_contents, to_input_position(range.start)),
                line_indices.line_column_to_safe_byte(file_contents, to_input_position(range.end)),
            ) else {
                break;
            };
            let (start, end) = (start as usize, end as usize);
            if start > end || end > previous_start {
                break;
            }
            result_len = result_len - (end - start) + change.text.len();
            edits.push((start, end, change.text.as_str()));
            previous_start = start;
        }
        if edits.len() == content_changes.len() {
            let mut text = String::with_capacity(result_len);
            let mut cursor = 0;
            for (start, end, replacement) in edits.into_iter().rev() {
                text.push_str(&file_contents[cursor..start]);
                text.push_str(replacement);
                cursor = end;
            }
            text.push_str(&file_contents[cursor..]);
            return Ok(text);
        }
    }

    // If at least one of the changes is a full document change, use the last
    // of them as the starting point and ignore all previous changes.
    let (mut text, content_changes, has_full_replacement) = match content_changes
        .iter()
        .rposition(|change| change.range.is_none())
    {
        Some(idx) => {
            let text = std::mem::take(&mut content_changes[idx].text);
            (text, &content_changes[idx + 1..], true)
        }
        None => (file_contents.to_string(), &content_changes[..], false),
    };
    if content_changes.is_empty() {
        return Ok(text);
    }

    let mut line_indices = if has_full_replacement {
        Cow::Owned(NewlineIndices::new())
    } else {
        Cow::Borrowed(line_indices)
    };
    // The changes we got must be applied sequentially, but can cross lines so we
    // have to keep our line index updated.
    // Some clients (e.g. Code) sort the ranges in reverse. As an optimization, we
    // remember the last valid line in the index and only rebuild it if needed.
    // The VFS will normalize the end of lines to `\n`.
    let mut index_valid = !0u32;
    for change in content_changes {
        // The None case can't happen as we have handled it above already
        if let Some(range) = change.range {
            if range.start > range.end {
                bail!("Invalid range of a change, start is bigger than end: {range:?}");
            }
            if range.end.line >= index_valid {
                line_indices = Cow::Owned(NewlineIndices::new());
            }
            index_valid = range.start.line;
            let start =
                line_indices.line_column_to_safe_byte(&text, to_input_position(range.start))?;
            let end = line_indices.line_column_to_safe_byte(&text, to_input_position(range.end))?;
            text.replace_range(start as usize..end as usize, &change.text);
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use lsp_types::Range;

    use super::*;

    fn simplified_apply_document_changes(
        file_contents: &str,
        change: Vec<TextDocumentContentChangeEvent>,
    ) -> anyhow::Result<String> {
        apply_document_changes(file_contents, &NewlineIndices::new(), change, |pos| {
            InputPosition::CodePoints {
                line: pos.line as usize,
                column: pos.character as usize,
            }
        })
    }

    fn expect_valid_apply_document_change(
        code: &str,
        change: TextDocumentContentChangeEvent,
    ) -> String {
        simplified_apply_document_changes(code, vec![change]).unwrap()
    }

    fn change(
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
        new_text: &str,
    ) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: start_line,
                    character: start_column,
                },
                end: Position {
                    line: end_line,
                    character: end_column,
                },
            }),
            range_length: None,
            text: new_text.into(),
        }
    }

    #[test]
    fn test_apply_change_valid() {
        assert_eq!(
            expect_valid_apply_document_change("", change(0, 0, 0, 0, "hello")),
            "hello"
        );
        assert_eq!(
            expect_valid_apply_document_change("x\ny\n", change(0, 0, 0, 0, "hello")),
            "hellox\ny\n"
        );
        assert_eq!(
            expect_valid_apply_document_change("x\ndeletethis\n", change(1, 0, 2, 0, "")),
            "x\n"
        );
    }

    #[test]
    fn test_apply_change_unicode() {
        assert_eq!(
            expect_valid_apply_document_change("äöü", change(0, 1, 0, 2, "hello")),
            "ähelloü"
        );
    }

    #[test]
    fn test_apply_single_change_utf16_surrogate_boundaries() {
        let apply = |change| {
            apply_document_changes("😀x\n", &NewlineIndices::new(), vec![change], |pos| {
                InputPosition::Utf16CodeUnits {
                    line: pos.line as usize,
                    column: pos.character as usize,
                }
            })
        };
        assert_eq!(apply(change(0, 2, 0, 3, "y")).unwrap(), "😀y\n");
        assert!(apply(change(0, 1, 0, 2, "y")).is_err());
        assert!(apply(change(0, 2, 0, 4, "y")).is_err());
    }

    #[test]
    fn test_apply_change_with_empty() {
        let empty = |new_text: &str| TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: new_text.into(),
        };
        assert_eq!(
            simplified_apply_document_changes("asdf\nb", vec![empty("")]).unwrap(),
            ""
        );
        assert_eq!(
            simplified_apply_document_changes(
                "asdf\nb",
                vec![
                    change(0, 0, 0, 0, "something"),
                    empty("new_code"),
                    change(0, 3, 0, 4, "-")
                ]
            )
            .unwrap(),
            "new-code"
        );
    }

    #[test]
    fn test_full_replacement_then_range_uses_replacement_lines() {
        let old_code = "a\nb\n";
        let old_lines = NewlineIndices::new();
        old_lines
            .line_column_to_safe_byte(old_code, InputPosition::CodePoints { line: 1, column: 0 })
            .unwrap();
        let full = TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "first\nsecond\n".into(),
        };
        assert_eq!(
            apply_document_changes(
                old_code,
                &old_lines,
                vec![full, change(1, 0, 1, 6, "final")],
                |pos| InputPosition::CodePoints {
                    line: pos.line as usize,
                    column: pos.character as usize,
                }
            )
            .unwrap(),
            "first\nfinal\n"
        );
    }

    #[test]
    fn test_full_replacement_then_reverse_ranges_use_replacement_lines() {
        let old_code = "a\nb\n";
        let old_lines = NewlineIndices::new();
        old_lines
            .line_column_to_safe_byte(old_code, InputPosition::CodePoints { line: 1, column: 0 })
            .unwrap();
        let full = TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "first\nsecond\n".into(),
        };
        let changes = vec![full, change(1, 0, 1, 1, "S"), change(0, 0, 0, 1, "F")];
        assert_eq!(
            apply_document_changes(old_code, &old_lines, changes, |pos| {
                InputPosition::CodePoints {
                    line: pos.line as usize,
                    column: pos.character as usize,
                }
            })
            .unwrap(),
            "First\nSecond\n"
        );
    }

    #[test]
    fn test_reverse_changes_with_multiline_replacement_and_utf16() {
        let tail = format!("#{}\n", "x".repeat(8192));
        let code = format!("😀x\nalpha\nbeta\n{tail}");
        let changes = vec![
            change(2, 0, 2, 4, "B\nC"),
            change(1, 0, 1, 5, "A"),
            change(0, 2, 0, 3, "y"),
        ];
        let result = apply_document_changes(&code, &NewlineIndices::new(), changes, |pos| {
            InputPosition::Utf16CodeUnits {
                line: pos.line as usize,
                column: pos.character as usize,
            }
        })
        .unwrap();
        assert_eq!(result, format!("😀y\nA\nB\nC\n{tail}"));
    }

    #[test]
    fn test_reverse_changes_with_same_position_insertions() {
        let tail = format!("#{}\n", "x".repeat(8192));
        let code = format!("xy\n{tail}");
        let changes = vec![change(0, 1, 0, 1, "A"), change(0, 1, 0, 1, "B")];
        assert_eq!(
            simplified_apply_document_changes(&code, changes).unwrap(),
            format!("xBAy\n{tail}")
        );
    }

    #[test]
    fn test_overlapping_changes_remain_sequential() {
        let changes = vec![change(0, 1, 0, 2, "X"), change(0, 1, 0, 2, "Y")];
        assert_eq!(
            simplified_apply_document_changes("abc", changes).unwrap(),
            "aYc"
        );
    }

    #[test]
    fn test_large_reverse_length_changing_batch() {
        let tail = format!("#{}\n", "x".repeat(8192));
        let code = format!("a\nb\n{tail}");
        let changes = vec![change(1, 0, 1, 1, "bb"), change(0, 0, 0, 1, "aa")];
        assert_eq!(
            simplified_apply_document_changes(&code, changes).unwrap(),
            format!("aa\nbb\n{tail}")
        );
    }

    #[test]
    fn test_large_overlapping_batch_remains_sequential() {
        let tail = format!("#{}\n", "x".repeat(8192));
        let code = format!("abc\n{tail}");
        let changes = vec![change(0, 1, 0, 2, "XX"), change(0, 1, 0, 3, "Y")];
        assert_eq!(
            simplified_apply_document_changes(&code, changes).unwrap(),
            format!("aYc\n{tail}")
        );
    }

    #[test]
    fn test_apply_change_out_of_bounds() {
        assert_eq!(
            simplified_apply_document_changes(
                "asdf\nb",
                vec![change(0, 0, 0, 0, ""), change(0, 4, 0, 5, "-")]
            )
            .unwrap_err()
            .to_string(),
            "Columns are out of bounds"
        );
        assert_eq!(
            simplified_apply_document_changes("asdf\n", vec![change(2, 0, 2, 0, "x")])
                .unwrap_err()
                .to_string(),
            "File has only 3 lines, but line 2 is requested"
        );
    }

    #[test]
    fn test_apply_change_invalid_reverse() {
        assert_eq!(
            simplified_apply_document_changes("x\n", vec![change(0, 1, 0, 0, "")])
                .unwrap_err()
                .to_string(),
            "Invalid range of a change, start is bigger than end: Range \
             { start: Position { line: 0, character: 1 }, end: Position { line: 0, character: 0 } }"
        );
        assert_eq!(
            simplified_apply_document_changes("x\n", vec![change(1, 0, 0, 1, "")])
                .unwrap_err()
                .to_string(),
            "Invalid range of a change, start is bigger than end: Range \
             { start: Position { line: 1, character: 0 }, end: Position { line: 0, character: 1 } }"
        );
    }
}
