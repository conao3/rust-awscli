//! `docutils/statemachine.py`: `StringList` and the line-moving part of
//! `StateMachine` / `StateMachineWS`.

use super::chars::{clen, is_space, is_wide, lstrip, slice, slice_from, slice_to, strip};

/// Python list indexing (negative indices count from the end).
pub(super) fn py_index(len: usize, i: isize) -> Option<usize> {
    if i < 0 {
        let j = len as isize + i;
        if j < 0 { None } else { Some(j as usize) }
    } else if (i as usize) < len {
        Some(i as usize)
    } else {
        None
    }
}

/// `StringList` (a `ViewList` of lines with their source offsets; the
/// source is always the one input string).
#[derive(Debug, Clone, Default)]
pub(super) struct StringList {
    pub data: Vec<String>,
    pub items: Vec<usize>,
}

/// `ViewList.info` outcome.
pub(super) enum Info {
    Offset(usize),
    PastEnd,
    Missing,
}

impl StringList {
    pub(super) fn new(lines: Vec<String>) -> StringList {
        let items = (0..lines.len()).collect();
        StringList { data: lines, items }
    }

    pub(super) fn len(&self) -> usize {
        self.data.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// `self[a:b]` (Python slice bounds, clamped).
    pub(super) fn slice(&self, a: usize, b: usize) -> StringList {
        let len = self.len();
        let a = a.min(len);
        let b = b.min(len).max(a);
        StringList {
            data: self.data[a..b].to_vec(),
            items: self.items[a..b].to_vec(),
        }
    }

    pub(super) fn slice_from(&self, a: usize) -> StringList {
        self.slice(a, self.len())
    }

    /// `ViewList.info`.
    pub(super) fn info(&self, i: isize) -> Info {
        match py_index(self.items.len(), i) {
            Some(j) => Info::Offset(self.items[j]),
            None => {
                if i >= 0
                    && i as usize == self.data.len()
                    && py_index(self.items.len(), i - 1).is_some()
                {
                    Info::PastEnd
                } else {
                    Info::Missing
                }
            }
        }
    }

    /// `StringList.trim_left`.
    pub(super) fn trim_left(&mut self, length: usize, start: usize, end: usize) {
        let end = end.min(self.data.len());
        for i in start.min(end)..end {
            self.data[i] = slice_from(&self.data[i], length).to_string();
        }
    }

    /// `ViewList.trim_start`.
    pub(super) fn trim_start(&mut self, n: usize) {
        self.data.drain(..n);
        self.items.drain(..n);
    }

    /// `ViewList.trim_end`.
    pub(super) fn trim_end(&mut self, n: usize) {
        let l = self.data.len();
        self.data.truncate(l - n);
        self.items.truncate(l - n);
    }

    /// `StringList.get_text_block`; `Err` is `UnexpectedIndentationError`
    /// with the block and the source line.
    pub(super) fn get_text_block(
        &self,
        start: usize,
        flush_left: bool,
    ) -> Result<StringList, (StringList, Option<usize>)> {
        let mut end = start;
        let last = self.data.len();
        while end < last {
            let line = &self.data[end];
            if strip(line).is_empty() {
                break;
            }
            if flush_left && line.starts_with(' ') {
                let off = self.items.get(end).copied();
                return Err((self.slice(start, end), off.map(|o| o + 1)));
            }
            end += 1;
        }
        Ok(self.slice(start, end))
    }

    /// `StringList.get_indented`.
    pub(super) fn get_indented(
        &self,
        start: usize,
        until_blank: bool,
        strip_indent: bool,
        block_indent: Option<usize>,
        first_indent: Option<usize>,
    ) -> (StringList, usize, bool) {
        let mut indent = block_indent;
        let mut end = start;
        let mut first_indent = first_indent;
        if block_indent.is_some() && first_indent.is_none() {
            first_indent = block_indent;
        }
        if first_indent.is_some() {
            end += 1;
        }
        let last = self.data.len();
        let mut blank_finish = true;
        while end < last {
            let line = &self.data[end];
            if !line.is_empty()
                && (!line.starts_with(' ')
                    || block_indent.is_some_and(|b| !strip(slice_to(line, b)).is_empty()))
            {
                blank_finish = end > start && strip(&self.data[end - 1]).is_empty();
                break;
            }
            let stripped = lstrip(line);
            if stripped.is_empty() {
                if until_blank {
                    blank_finish = true;
                    break;
                }
            } else if block_indent.is_none() {
                let line_indent = clen(line) - clen(stripped);
                indent = Some(match indent {
                    None => line_indent,
                    Some(i) => i.min(line_indent),
                });
            }
            end += 1;
        }
        let mut block = self.slice(start, end);
        if let Some(fi) = first_indent
            && !block.is_empty()
        {
            block.data[0] = slice_from(&block.data[0], fi).to_string();
        }
        let ind = indent.unwrap_or(0);
        if ind > 0 && strip_indent {
            let s = usize::from(first_indent.is_some());
            let l = block.len();
            block.trim_left(ind, s, l);
        }
        (block, ind, blank_finish)
    }

    /// `StringList.get_2D_block`.
    pub(super) fn get_2d_block(
        &self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    ) -> StringList {
        let mut block = self.slice(top, bottom);
        let mut indent = right;
        let mut left = left;
        let mut right = right;
        for i in 0..block.data.len() {
            let line = block.data[i].clone();
            let ci = column_indices(&line);
            let n = clen(&line);
            left = match ci.get(left) {
                Some(&v) => v,
                None => left + n - ci.len(),
            };
            right = match ci.get(right) {
                Some(&v) => v,
                None => right + n - ci.len(),
            };
            let cell = slice(&line, left, right);
            let cell = cell.trim_end_matches(is_space).to_string();
            if !cell.is_empty() {
                indent = indent.min(clen(&cell) - clen(lstrip(&cell)));
            }
            block.data[i] = cell;
        }
        if 0 < indent && indent < right {
            for l in &mut block.data {
                *l = slice_from(l, indent).to_string();
            }
        }
        block
    }

    /// `StringList.pad_double_width`.
    pub(super) fn pad_double_width(&mut self, pad: char) {
        for l in &mut self.data {
            let mut new = String::new();
            for c in l.chars() {
                new.push(c);
                if is_wide(c) {
                    new.push(pad);
                }
            }
            *l = new;
        }
    }

    /// `StringList.replace`.
    pub(super) fn replace(&mut self, old: &str, new: &str) {
        for l in &mut self.data {
            *l = l.replace(old, new);
        }
    }
}

/// `utils.column_indices`.
fn column_indices(text: &str) -> Vec<usize> {
    text.chars()
        .enumerate()
        .filter(|(_, c)| !super::chars::is_combining(*c))
        .map(|(i, _)| i)
        .collect()
}

/// `statemachine.string2lines` with `convert_whitespace=True`.
pub(super) fn string2lines(s: &str, tab_width: usize) -> Vec<String> {
    let s: String = s
        .chars()
        .map(|c| if c == '\x0b' || c == '\x0c' { ' ' } else { c })
        .collect();
    super::chars::splitlines(&s)
        .into_iter()
        .map(|l| {
            super::chars::expandtabs(&l, tab_width)
                .trim_end_matches(is_space)
                .to_string()
        })
        .collect()
}
