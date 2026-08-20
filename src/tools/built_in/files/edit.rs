use std::ops::Range;

use unicode_normalization::UnicodeNormalization as _;

use super::FileEdit;

#[derive(Debug)]
pub(super) struct EditResult {
    pub content: String,
    pub normalized_match: bool,
}

struct Match {
    edit: usize,
    range: Range<usize>,
    replacement: String,
}

pub(super) fn apply(original: &str, path: &str, edits: &[FileEdit]) -> Result<EditResult, String> {
    if edits.is_empty() {
        return Err("edits must contain at least one replacement".to_string());
    }

    let (bom, original) = original
        .strip_prefix('\u{feff}')
        .map_or(("", original), |content| ("\u{feff}", content));
    let line_ending = detect_line_ending(original);
    let original = normalize_line_endings(original);
    let normalized = normalize_for_match(&original);

    let mut use_normalized_match = false;
    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("edits[{index}].old_text must not be empty"));
        }
        let old_text = normalize_line_endings(&edit.old_text);
        if !original.contains(&old_text) {
            if !normalized.contains(&normalize_for_match(&old_text)) {
                return Err(format!(
                    "edits[{index}].old_text was not found in {path}; read the file or use search_files first"
                ));
            }
            use_normalized_match = true;
        }
    }

    let base = if use_normalized_match {
        &normalized
    } else {
        &original
    };
    let mut matches = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        let old_text = normalize_line_endings(&edit.old_text);
        let old_text = if use_normalized_match {
            normalize_for_match(&old_text)
        } else {
            old_text
        };
        let positions = base
            .match_indices(&old_text)
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        if positions.len() != 1 {
            return Err(format!(
                "edits[{index}].old_text occurs {} times in {path}; include enough context to make it unique",
                positions.len()
            ));
        }
        let start = positions[0];
        matches.push(Match {
            edit: index,
            range: start..start + old_text.len(),
            replacement: normalize_line_endings(&edit.new_text),
        });
    }

    matches.sort_by_key(|matched| matched.range.start);
    for pair in matches.windows(2) {
        if pair[0].range.end > pair[1].range.start {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}",
                pair[0].edit, pair[1].edit
            ));
        }
    }

    let edited = if use_normalized_match {
        apply_preserving_unchanged_lines(&original, base, &matches)?
    } else {
        apply_matches(base, &matches, 0)
    };
    if edited == original {
        return Err(format!("edits produced no changes to {path}"));
    }

    let edited = restore_line_endings(&edited, line_ending);
    Ok(EditResult {
        content: format!("{bom}{edited}"),
        normalized_match: use_normalized_match,
    })
}

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
}

fn detect_line_ending(content: &str) -> LineEnding {
    content.find('\n').map_or(LineEnding::Lf, |index| {
        if index > 0 && content.as_bytes()[index - 1] == b'\r' {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    })
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn restore_line_endings(text: &str, line_ending: LineEnding) -> String {
    match line_ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

fn normalize_for_match(text: &str) -> String {
    let normalized = text.nfkc().collect::<String>();
    let mut result = String::with_capacity(normalized.len());
    for line in normalized.split_inclusive('\n') {
        let (line, newline) = line
            .strip_suffix('\n')
            .map_or((line, false), |line| (line, true));
        result.extend(line.trim_end().chars().map(normalize_character));
        if newline {
            result.push('\n');
        }
    }
    result
}

fn normalize_character(character: char) -> char {
    match character {
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{00a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
        character => character,
    }
}

fn apply_matches(content: &str, matches: &[Match], offset: usize) -> String {
    let mut result = content.to_string();
    for matched in matches.iter().rev() {
        result.replace_range(
            matched.range.start - offset..matched.range.end - offset,
            &matched.replacement,
        );
    }
    result
}

fn line_ranges(content: &str) -> Vec<Range<usize>> {
    let mut offset = 0;
    content
        .split_inclusive('\n')
        .map(|line| {
            let range = offset..offset + line.len();
            offset = range.end;
            range
        })
        .collect()
}

fn matched_line_range(lines: &[Range<usize>], matched: &Match) -> Result<Range<usize>, String> {
    let start = lines
        .iter()
        .position(|line| matched.range.start >= line.start && matched.range.start < line.end)
        .ok_or_else(|| "edit range is outside the file".to_string())?;
    let mut end = start;
    while lines[end].end < matched.range.end {
        end += 1;
        if end == lines.len() {
            return Err("edit range is outside the file".to_string());
        }
    }
    Ok(start..end + 1)
}

fn apply_preserving_unchanged_lines(
    original: &str,
    normalized: &str,
    matches: &[Match],
) -> Result<String, String> {
    let original_lines = line_ranges(original);
    let normalized_lines = line_ranges(normalized);
    if original_lines.len() != normalized_lines.len() {
        return Err("normalization changed the file's line structure".to_string());
    }

    struct Group<'a> {
        lines: Range<usize>,
        matches: Vec<&'a Match>,
    }

    let mut groups: Vec<Group<'_>> = Vec::new();
    for matched in matches {
        let lines = matched_line_range(&normalized_lines, matched)?;
        if let Some(group) = groups.last_mut()
            && lines.start < group.lines.end
        {
            group.lines.end = group.lines.end.max(lines.end);
            group.matches.push(matched);
        } else {
            groups.push(Group {
                lines,
                matches: vec![matched],
            });
        }
    }

    let mut result = String::new();
    let mut original_line = 0;
    for group in groups {
        append_lines(
            &mut result,
            original,
            &original_lines,
            original_line..group.lines.start,
        );
        let start = normalized_lines[group.lines.start].start;
        let end = normalized_lines[group.lines.end - 1].end;
        let matches = group
            .matches
            .into_iter()
            .map(|matched| Match {
                edit: matched.edit,
                range: matched.range.clone(),
                replacement: matched.replacement.clone(),
            })
            .collect::<Vec<_>>();
        result.push_str(&apply_matches(&normalized[start..end], &matches, start));
        original_line = group.lines.end;
    }
    append_lines(
        &mut result,
        original,
        &original_lines,
        original_line..original_lines.len(),
    );
    Ok(result)
}

fn append_lines(output: &mut String, content: &str, lines: &[Range<usize>], range: Range<usize>) {
    for line in &lines[range] {
        output.push_str(&content[line.clone()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacement(old_text: &str, new_text: &str) -> FileEdit {
        FileEdit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn applies_disjoint_edits_against_the_original() {
        let result = apply(
            "alpha beta gamma",
            "notes.txt",
            &[replacement("alpha", "one"), replacement("gamma", "three")],
        )
        .unwrap();

        assert_eq!(result.content, "one beta three");
        assert!(!result.normalized_match);
    }

    #[test]
    fn rejects_overlapping_edits() {
        let error = apply(
            "alpha beta",
            "notes.txt",
            &[replacement("alpha beta", "one"), replacement("beta", "two")],
        )
        .unwrap_err();

        assert!(error.contains("overlap"));
    }

    #[test]
    fn normalized_match_preserves_bom_line_endings_and_untouched_lines() {
        let result = apply(
            "\u{feff}title  \r\n\u{201c}hello\u{201d}\u{2014}world\r\nuntouched  \r\n",
            "notes.txt",
            &[replacement("title\n\"hello\"-world", "changed")],
        )
        .unwrap();

        assert_eq!(result.content, "\u{feff}changed\r\nuntouched  \r\n");
        assert!(result.normalized_match);
    }

    #[test]
    fn requires_each_edit_to_be_unique() {
        let error = apply(
            "same\nsame\n",
            "notes.txt",
            &[replacement("same", "different")],
        )
        .unwrap_err();

        assert!(error.contains("occurs 2 times"));
    }
}
