//! Tables: `Body.table`, `Body.isolate_*_table`, `Body.build_table`
//! (`docutils/parsers/rst/states.py`) and
//! `docutils/parsers/rst/tableparser.py`.

use std::collections::BTreeMap;

use super::chars::{clen, is_combining, rstrip, slice, slice_from, strip};
use super::nodes::{Id, Tag, Val};
use super::parser::{MsgArgs, P, R, Sm};
use super::pyre::Regex;
use super::statemachine::StringList;

const PAD: char = '\x00';

/// `TableMarkupError`: message and offset.
struct TableError(String, usize);

/// A cell: (morerows, morecols, offset, block).
type Cell = (usize, usize, usize, StringList);
type TableData = (Vec<usize>, Vec<Vec<Option<Cell>>>, Vec<Vec<Option<Cell>>>);

fn ch(line: &str, i: usize) -> char {
    line.chars().nth(i).unwrap_or('\0')
}

fn update(master: &mut BTreeMap<usize, Vec<usize>>, new: BTreeMap<usize, Vec<usize>>) {
    for (k, v) in new {
        master.entry(k).or_default().extend(v);
    }
}

/// `GridTableParser`.
struct Grid {
    block: StringList,
    bottom: usize,
    right: usize,
    head_body_sep: Option<usize>,
    done: Vec<isize>,
    cells: Vec<(usize, usize, usize, usize, StringList)>,
    rowseps: BTreeMap<usize, Vec<usize>>,
    colseps: BTreeMap<usize, Vec<usize>>,
}

fn find_head_body_sep(block: &mut StringList, pat: &Regex) -> Result<Option<usize>, TableError> {
    let mut sep: Option<usize> = None;
    let mut i = 0;
    for j in 0..block.len() {
        i = j;
        let line = block.data[j].clone();
        if pat.is_match(&line) {
            if let Some(s) = sep.filter(|s| *s > 0) {
                return Err(TableError(
                    format!(
                        "Multiple head/body row separators (table lines {} and {}); only one allowed.",
                        s + 1,
                        j + 1
                    ),
                    j,
                ));
            }
            sep = Some(j);
            block.data[j] = line.replace('=', "-");
        }
    }
    if sep == Some(0) || sep == Some(block.len().wrapping_sub(1)) {
        return Err(TableError(
            "The head/body row separator may not be the first or last line of the table.".into(),
            i,
        ));
    }
    Ok(sep)
}

impl Grid {
    fn parse(block: &StringList) -> Result<TableData, TableError> {
        let mut block = block.clone();
        let w = clen(&block.data[0]);
        let mut g = Grid {
            bottom: block.len() - 1,
            right: w - 1,
            head_body_sep: None,
            done: vec![-1; w],
            cells: Vec::new(),
            rowseps: BTreeMap::from([(0, vec![0])]),
            colseps: BTreeMap::from([(0, vec![0])]),
            block: StringList::default(),
        };
        g.head_body_sep = find_head_body_sep(&mut block, &Regex::new(r"\+=[=+]+=\+ *$", false))?;
        g.block = block;
        g.parse_table()?;
        Ok(g.structure_from_cells())
    }

    fn parse_table(&mut self) -> Result<(), TableError> {
        let mut corners = vec![(0usize, 0usize)];
        while !corners.is_empty() {
            let (top, left) = corners.remove(0);
            if top == self.bottom || left == self.right || top as isize <= self.done[left] {
                continue;
            }
            let Some((bottom, right, rowseps, colseps)) = self.scan_cell(top, left) else {
                continue;
            };
            update(&mut self.rowseps, rowseps);
            update(&mut self.colseps, colseps);
            for col in left..right {
                self.done[col] = bottom as isize - 1;
            }
            let mut cellblock = self.block.get_2d_block(top + 1, left + 1, bottom, right);
            cellblock.replace("\x00", "");
            self.cells.push((top, left, bottom, right, cellblock));
            corners.push((top, right));
            corners.push((bottom, left));
            corners.sort();
        }
        let last = self.bottom as isize - 1;
        if (0..self.right).any(|c| self.done[c] != last) {
            return Err(TableError("Malformed table; parse incomplete.".into(), 0));
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn scan_cell(
        &self,
        top: usize,
        left: usize,
    ) -> Option<(
        usize,
        usize,
        BTreeMap<usize, Vec<usize>>,
        BTreeMap<usize, Vec<usize>>,
    )> {
        let mut colseps = BTreeMap::new();
        let line = &self.block.data[top];
        for i in left + 1..=self.right {
            let c = ch(line, i);
            if c == '+' {
                colseps.insert(i, vec![top]);
                if let Some((bottom, rowseps, newcolseps)) = self.scan_down(top, left, i) {
                    update(&mut colseps, newcolseps);
                    return Some((bottom, i, rowseps, colseps));
                }
            } else if c != '-' {
                return None;
            }
        }
        None
    }

    #[allow(clippy::type_complexity)]
    fn scan_down(
        &self,
        top: usize,
        left: usize,
        right: usize,
    ) -> Option<(
        usize,
        BTreeMap<usize, Vec<usize>>,
        BTreeMap<usize, Vec<usize>>,
    )> {
        let mut rowseps = BTreeMap::new();
        for i in top + 1..=self.bottom {
            let c = ch(&self.block.data[i], right);
            if c == '+' {
                rowseps.insert(i, vec![right]);
                if let Some((newrowseps, colseps)) = self.scan_left(top, left, i, right) {
                    update(&mut rowseps, newrowseps);
                    return Some((i, rowseps, colseps));
                }
            } else if c != '|' {
                return None;
            }
        }
        None
    }

    #[allow(clippy::type_complexity)]
    fn scan_left(
        &self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    ) -> Option<(BTreeMap<usize, Vec<usize>>, BTreeMap<usize, Vec<usize>>)> {
        let mut colseps = BTreeMap::new();
        let line = &self.block.data[bottom];
        for i in (left + 1..right).rev() {
            let c = ch(line, i);
            if c == '+' {
                colseps.insert(i, vec![bottom]);
            } else if c != '-' {
                return None;
            }
        }
        if ch(line, left) != '+' {
            return None;
        }
        let mut rowseps = BTreeMap::new();
        for i in (top + 1..bottom).rev() {
            let c = ch(&self.block.data[i], left);
            if c == '+' {
                rowseps.insert(i, vec![left]);
            } else if c != '|' {
                return None;
            }
        }
        Some((rowseps, colseps))
    }

    fn structure_from_cells(&self) -> TableData {
        let rowseps: Vec<usize> = self.rowseps.keys().copied().collect();
        let colseps: Vec<usize> = self.colseps.keys().copied().collect();
        let rowindex = |k: usize| rowseps.iter().position(|&r| r == k).unwrap();
        let colindex = |k: usize| colseps.iter().position(|&c| c == k).unwrap();
        let colspecs: Vec<usize> = (1..colseps.len())
            .map(|i| colseps[i] - colseps[i - 1] - 1)
            .collect();
        let mut rows: Vec<Vec<Option<Cell>>> =
            vec![vec![None; colseps.len() - 1]; rowseps.len() - 1];
        for (top, left, bottom, right, block) in &self.cells {
            let rownum = rowindex(*top);
            let colnum = colindex(*left);
            let morerows = rowindex(*bottom) - rownum - 1;
            let morecols = colindex(*right) - colnum - 1;
            rows[rownum][colnum] = Some((morerows, morecols, top + 1, block.clone()));
        }
        match self.head_body_sep {
            Some(s) if s > 0 => {
                let n = rowindex(s);
                let body = rows.split_off(n);
                (colspecs, rows, body)
            }
            _ => (colspecs, Vec::new(), rows),
        }
    }
}

/// `SimpleTableParser`.
struct Simple {
    block: StringList,
    head_body_sep: Option<usize>,
    columns: Vec<(usize, usize)>,
    border_end: usize,
    table: Vec<Vec<(usize, usize, usize, StringList)>>,
}

impl Simple {
    fn parse(block: &StringList) -> Result<TableData, TableError> {
        let mut block = block.clone();
        block.data[0] = block.data[0].replace('=', "-");
        let l = block.len() - 1;
        block.data[l] = block.data[l].replace('=', "-");
        let sep = find_head_body_sep(&mut block, &Regex::new("=[ =]*$", false))?;
        let mut s = Simple {
            block,
            head_body_sep: sep,
            columns: Vec::new(),
            border_end: 0,
            table: Vec::new(),
        };
        s.parse_table()?;
        Ok(s.structure_from_cells())
    }

    fn parse_table(&mut self) -> Result<(), TableError> {
        let first = self.block.data[0].clone();
        self.columns = self.parse_columns(&first, 0)?;
        self.border_end = self.columns.last().unwrap().1;
        let (firststart, firstend) = self.columns[0];
        let mut offset = 1;
        let mut start = 1;
        let mut text_found = false;
        let span = Regex::new("-[ -]*$", false);
        while offset < self.block.len() {
            let line = self.block.data[offset].clone();
            if span.is_match(&line) {
                let lines = self.block.slice(start, offset);
                self.parse_row(&lines, start, Some((rstrip(&line).to_string(), offset)))?;
                start = offset + 1;
                text_found = false;
            } else if !strip(slice(&line, firststart, firstend)).is_empty() {
                if text_found && offset != start {
                    let lines = self.block.slice(start, offset);
                    self.parse_row(&lines, start, None)?;
                }
                start = offset;
                text_found = true;
            } else if !text_found {
                start = offset + 1;
            }
            offset += 1;
        }
        Ok(())
    }

    fn parse_columns(
        &mut self,
        line: &str,
        offset: usize,
    ) -> Result<Vec<(usize, usize)>, TableError> {
        let cs: Vec<char> = line.chars().collect();
        let mut cols = Vec::new();
        let mut end = 0usize;
        loop {
            let begin = cs[end.min(cs.len())..]
                .iter()
                .position(|&c| c == '-')
                .map(|p| p + end);
            let Some(begin) = begin else { break };
            end = cs[begin..]
                .iter()
                .position(|&c| c == ' ')
                .map(|p| p + begin)
                .unwrap_or(cs.len());
            cols.push((begin, end));
        }
        if !self.columns.is_empty() {
            if cols.last().unwrap().1 != self.border_end {
                return Err(TableError(
                    format!("Column span incomplete in table line {}.", offset + 1),
                    offset,
                ));
            }
            let n = cols.len() - 1;
            cols[n] = (cols[n].0, self.columns.last().unwrap().1);
        }
        Ok(cols)
    }

    fn init_row(
        &self,
        colspec: &[(usize, usize)],
        offset: usize,
    ) -> Result<Vec<(usize, usize, usize, StringList)>, TableError> {
        let mut i = 0;
        let mut cells = Vec::new();
        for &(start, end) in colspec {
            let mut morecols = 0;
            let err = || {
                TableError(
                    format!(
                        "Column span alignment problem in table line {}.",
                        offset + 2
                    ),
                    offset + 1,
                )
            };
            if self.columns.get(i).map(|c| c.0) != Some(start) {
                return Err(err());
            }
            while self.columns.get(i).ok_or_else(err)?.1 != end {
                i += 1;
                morecols += 1;
                if i >= self.columns.len() {
                    return Err(err());
                }
            }
            cells.push((0, morecols, offset, StringList::default()));
            i += 1;
        }
        Ok(cells)
    }

    fn parse_row(
        &mut self,
        lines: &StringList,
        start: usize,
        spanline: Option<(String, usize)>,
    ) -> Result<(), TableError> {
        if lines.is_empty() && spanline.is_none() {
            return Ok(());
        }
        let mut columns = match &spanline {
            Some((l, o)) => self.parse_columns(l, *o)?,
            None => self.columns.clone(),
        };
        self.check_columns(lines, start, &mut columns)?;
        let mut row = self.init_row(&columns, start)?;
        for (i, &(s, e)) in columns.iter().enumerate() {
            let mut cellblock = lines.get_2d_block(0, s, lines.len(), e);
            cellblock.replace("\x00", "");
            row[i].3 = cellblock;
        }
        self.table.push(row);
        Ok(())
    }

    fn check_columns(
        &mut self,
        lines: &StringList,
        first_line: usize,
        columns: &mut Vec<(usize, usize)>,
    ) -> Result<(), TableError> {
        columns.push((usize::MAX, 0));
        let lastcol = columns.len() - 2;
        let lines: Vec<String> = lines
            .data
            .iter()
            .map(|l| l.chars().filter(|c| !is_combining(*c)).collect())
            .collect();
        for i in 0..columns.len() - 1 {
            let (start, end) = columns[i];
            let nextstart = columns[i + 1].0;
            for (offset, line) in lines.iter().enumerate() {
                if i == lastcol && !strip(slice_from(line, end)).is_empty() {
                    let text = rstrip(slice_from(line, start));
                    let new_end = start + clen(text);
                    let (main_start, main_end) = *self.columns.last().unwrap();
                    columns[i] = (start, main_end.max(new_end));
                    if new_end > main_end {
                        let n = self.columns.len() - 1;
                        self.columns[n] = (main_start, new_end);
                    }
                } else if !strip(slice(line, end, nextstart)).is_empty() {
                    return Err(TableError(
                        format!(
                            "Text in column margin in table line {}.",
                            first_line + offset + 1
                        ),
                        first_line + offset,
                    ));
                }
            }
        }
        columns.pop();
        Ok(())
    }

    fn structure_from_cells(&self) -> TableData {
        let colspecs: Vec<usize> = self.columns.iter().map(|(s, e)| e - s).collect();
        let mut first_body_row = 0;
        if let Some(sep) = self.head_body_sep.filter(|s| *s > 0) {
            for (i, row) in self.table.iter().enumerate() {
                if row[0].2 > sep {
                    first_body_row = i;
                    break;
                }
            }
        }
        let conv = |rows: &[Vec<(usize, usize, usize, StringList)>]| -> Vec<Vec<Option<Cell>>> {
            rows.iter()
                .map(|r| r.iter().map(|c| Some(c.clone())).collect())
                .collect()
        };
        (
            colspecs,
            conv(&self.table[..first_body_row]),
            conv(&self.table[first_body_row..]),
        )
    }
}

impl P {
    /// `Body.table`.
    pub(super) fn table(&mut self, sm: &mut Sm, grid: bool) -> R<(Vec<Id>, bool)> {
        let (block, messages, bf) = if grid {
            self.isolate_grid_table(sm)?
        } else {
            self.isolate_simple_table(sm)?
        };
        let nodelist = match block {
            Some(block) => {
                let parsed = if grid {
                    Grid::parse(&block)
                } else {
                    Simple::parse(&block)
                };
                match parsed {
                    Ok(tabledata) => {
                        let tableline = sm.abs_line_number() - block.len() as isize + 1;
                        let table = self.build_table(tabledata, tableline)?;
                        let mut v = vec![table];
                        v.extend(messages);
                        v
                    }
                    Err(TableError(msg, offset)) => {
                        let mut v = self.malformed_table(sm, &block, &msg, offset)?;
                        v.extend(messages);
                        v
                    }
                }
            }
            None => messages,
        };
        Ok((nodelist, bf))
    }

    fn isolate_grid_table(&mut self, sm: &mut Sm) -> R<(Option<StringList>, Vec<Id>, bool)> {
        let mut messages = Vec::new();
        let mut bf = true;
        let mut block = match self.get_text_block(sm, true)? {
            Ok(b) => b,
            Err((b, srcline)) => {
                messages.push(self.error(
                    "Unexpected indentation.",
                    MsgArgs {
                        source: Some("<string>".into()),
                        line: srcline.map(|l| l as isize),
                        ..Default::default()
                    },
                )?);
                bf = false;
                b
            }
        };
        block.pad_double_width(PAD);
        let width = clen(strip(&block.data[0]));
        for i in 0..block.len() {
            block.data[i] = strip(&block.data[i]).to_string();
            let c = block.data[i].chars().next();
            if !matches!(c, Some('+' | '|')) {
                bf = false;
                let n = (block.len() - i) as isize;
                self.previous_line(sm, n);
                block = block.slice(0, i);
                break;
            }
        }
        let top = Regex::new(r"\+-[-+]+-\+ *$", false);
        if block.is_empty() || !top.is_match(block.data.last().unwrap()) {
            bf = false;
            let mut found = false;
            let mut i = block.len() as isize - 2;
            while i > 1 {
                if top.is_match(&block.data[i as usize]) {
                    let n = block.len() as isize - i + 1;
                    self.previous_line(sm, n);
                    block = block.slice(0, i as usize + 1);
                    found = true;
                    break;
                }
                i -= 1;
            }
            if !found {
                messages.extend(self.malformed_table(sm, &block, "", 0)?);
                return Ok((None, messages, bf));
            }
        }
        for i in 0..block.len() {
            let l = &block.data[i];
            if clen(l) != width || !matches!(l.chars().last(), Some('+' | '|')) {
                messages.extend(self.malformed_table(sm, &block, "", 0)?);
                return Ok((None, messages, bf));
            }
        }
        Ok((Some(block), messages, bf))
    }

    fn isolate_simple_table(&mut self, sm: &mut Sm) -> R<(Option<StringList>, Vec<Id>, bool)> {
        let start = sm.line_offset as usize;
        let lines = sm.lines.clone();
        let limit = lines.len() - 1;
        let toplen = clen(strip(&lines.data[start]));
        let border = &self.pats.simple_table_border;
        let mut found = 0;
        let mut found_at = None;
        let mut i = start + 1;
        let mut end = None;
        while i <= limit {
            let line = &lines.data[i];
            if border.is_match(line) {
                if clen(strip(line)) != toplen {
                    self.next_line(sm, (i - start) as isize).ok();
                    let block = lines.slice(start, i + 1);
                    let messages = self.malformed_table(
                        sm,
                        &block,
                        "Bottom/header table border does not match top border.",
                        0,
                    )?;
                    let bf = i == limit || strip(&lines.data[i + 1]).is_empty();
                    return Ok((None, messages, bf));
                }
                found += 1;
                found_at = Some(i);
                if found == 2 || i == limit || strip(&lines.data[i + 1]).is_empty() {
                    end = Some(i);
                    break;
                }
            }
            i += 1;
        }
        let Some(end) = end else {
            let (extra, block) = if let Some(fa) = found_at {
                self.next_line(sm, (fa - start) as isize).ok();
                (
                    " or no blank line after table bottom",
                    lines.slice(start, fa + 1),
                )
            } else {
                self.next_line(sm, (i - start) as isize - 1).ok();
                ("", lines.slice_from(start))
            };
            let messages = self.malformed_table(
                sm,
                &block,
                &format!("No bottom table border found{extra}."),
                0,
            )?;
            return Ok((None, messages, extra.is_empty()));
        };
        self.next_line(sm, (end - start) as isize).ok();
        let mut block = lines.slice(start, end + 1);
        block.pad_double_width(PAD);
        let bf = end == limit || strip(&lines.data[end + 1]).is_empty();
        Ok((Some(block), Vec::new(), bf))
    }

    /// `Body.malformed_table`.
    fn malformed_table(
        &mut self,
        sm: &Sm,
        block: &StringList,
        detail: &str,
        offset: usize,
    ) -> R<Vec<Id>> {
        let mut block = block.clone();
        block.replace("\x00", "");
        let data = block.data.join("\n");
        let mut message = "Malformed table.".to_string();
        let startline = sm.abs_line_number() - block.len() as isize + 1;
        if !detail.is_empty() {
            message.push('\n');
            message.push_str(detail);
        }
        let lb = self.literal(&data);
        let e = self.error(
            &message,
            MsgArgs::line(startline + offset as isize).child(lb),
        )?;
        Ok(vec![e])
    }

    /// `Body.build_table`.
    fn build_table(&mut self, data: TableData, tableline: isize) -> R<Id> {
        let (colwidths, headrows, bodyrows) = data;
        let table = self.doc.elem(Tag::Table, "");
        let tgroup = self.doc.elem(Tag::Tgroup, "");
        self.doc
            .set(tgroup, "cols", Val::Int(colwidths.len() as i64));
        self.doc.append(table, tgroup);
        for w in colwidths {
            let cs = self.doc.elem(Tag::Colspec, "");
            self.doc.set(cs, "colwidth", Val::Int(w as i64));
            self.doc.append(tgroup, cs);
        }
        if !headrows.is_empty() {
            let thead = self.doc.elem(Tag::Thead, "");
            self.doc.append(tgroup, thead);
            for row in headrows {
                let r = self.build_table_row(row, tableline)?;
                self.doc.append(thead, r);
            }
        }
        let tbody = self.doc.elem(Tag::Tbody, "");
        self.doc.append(tgroup, tbody);
        for row in bodyrows {
            let r = self.build_table_row(row, tableline)?;
            self.doc.append(tbody, r);
        }
        Ok(table)
    }

    fn build_table_row(&mut self, rowdata: Vec<Option<Cell>>, tableline: isize) -> R<Id> {
        let row = self.doc.elem(Tag::Row, "");
        for cell in rowdata.into_iter().flatten() {
            let (morerows, morecols, offset, cellblock) = cell;
            let entry = self.doc.elem(Tag::Entry, "");
            if morerows > 0 {
                self.doc.set(entry, "morerows", Val::Int(morerows as i64));
            }
            if morecols > 0 {
                self.doc.set(entry, "morecols", Val::Int(morecols as i64));
            }
            self.doc.append(row, entry);
            if !cellblock.data.concat().is_empty() {
                self.nested_parse(cellblock, tableline + offset as isize, entry, false)?;
            }
        }
        Ok(row)
    }
}
