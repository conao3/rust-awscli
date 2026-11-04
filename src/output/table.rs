//! The table output format (`awscli/table.py` and `TableFormatter` in
//! `awscli/formatter.py`).

use crate::pyfmt::py_str;
use crate::value::Value;

use super::text::type_name;
use super::width_table::WIDE;

/// Width of one character: 2 when `unicodedata.east_asian_width` is W, F,
/// or A, else 1.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    let wide = WIDE
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok();
    if wide { 2 } else { 1 }
}

/// `table.get_text_length`.
pub fn get_text_length(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn spaces(n: i64) -> String {
    " ".repeat(n.max(0) as usize)
}

/// `table.center_text`.
pub fn center_text(
    text: &str,
    length: i64,
    left_edge: &str,
    right_edge: &str,
    text_length: Option<usize>,
) -> String {
    let text_length = text_length.unwrap_or_else(|| get_text_length(text)) as i64;
    let char_start = length.div_euclid(2) - text_length.div_euclid(2) - 1;
    let mut output = String::new();
    output.push_str(left_edge);
    output.push_str(&spaces(char_start));
    output.push_str(text);
    let length_so_far = get_text_length(left_edge) as i64 + char_start + text_length;
    let right_side_spaces = length - get_text_length(right_edge) as i64 - length_so_far;
    output.push_str(&spaces(right_side_spaces));
    output.push_str(right_edge);
    output
}

/// `table.align_left` (with the default `left_padding=2`).
pub fn align_left(
    text: &str,
    length: i64,
    left_edge: &str,
    right_edge: &str,
    text_length: Option<usize>,
) -> String {
    let left_padding = 2;
    let text_length = text_length.unwrap_or_else(|| get_text_length(text)) as i64;
    let computed_length = text_length
        + left_padding
        + get_text_length(left_edge) as i64
        + get_text_length(right_edge) as i64;
    let padding = if length - computed_length >= 0 {
        left_padding
    } else {
        0
    };
    let left_len = left_edge.chars().count() as i64;
    let right_len = right_edge.chars().count() as i64;
    let length_so_far = left_len + padding + text_length;
    let mut output = String::new();
    output.push_str(left_edge);
    output.push_str(&spaces(padding));
    output.push_str(text);
    output.push_str(&spaces(length - length_so_far - right_len));
    output.push_str(right_edge);
    output
}

/// `table.Styler` / `table.ColorizedStyler` (colorama codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Styler {
    Plain,
    Colorized,
}

const RESET_ALL: &str = "\x1b[0m";

impl Styler {
    fn style_title(self, text: &str) -> String {
        text.to_string()
    }

    fn style_header_column(self, text: &str) -> String {
        text.to_string()
    }

    fn style_row_element(self, text: &str) -> String {
        match self {
            Styler::Plain => text.to_string(),
            Styler::Colorized => format!("\x1b[1m\x1b[34m{text}{RESET_ALL}"),
        }
    }

    fn style_indentation_char(self, text: &str) -> String {
        match self {
            Styler::Plain => text.to_string(),
            Styler::Colorized => format!("\x1b[2m\x1b[33m{text}{RESET_ALL}"),
        }
    }
}

/// The stream the table is written to. With `autoreset`, every `write`
/// call is followed by `Style.RESET_ALL`, as colorama's `AnsiToWin32`
/// wrapper around `sys.stdout` does once `colorama.init(autoreset=True)`
/// has run.
pub struct Stream<'a> {
    pub out: &'a mut String,
    pub autoreset: bool,
}

impl Stream<'_> {
    fn write(&mut self, text: &str) {
        self.out.push_str(text);
        if self.autoreset {
            self.out.push_str(RESET_ALL);
        }
    }
}

/// `table.IndentedStream.write`.
fn indented_write(
    stream: &mut Stream<'_>,
    indent_level: usize,
    left: &str,
    right: &str,
    text: &str,
) {
    stream.write(&left.repeat(indent_level));
    if let Some(stripped) = text.strip_suffix('\n') {
        stream.write(stripped);
        stream.write(&right.repeat(indent_level));
        stream.write("\n");
    } else {
        stream.write(text);
    }
}

/// `table.Section`.
#[derive(Debug, Clone, Default)]
pub struct Section {
    pub title: String,
    /// The title is `None` (`new_section(None)`): it renders like an empty
    /// title but `get_text_length(None)` counts `str(None)`.
    pub title_is_none: bool,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub indent_level: usize,
    num_cols: Option<usize>,
    max_widths: Vec<usize>,
}

impl Section {
    /// `Section.calculate_column_widths`.
    pub fn calculate_column_widths(&self, padding: usize, max_width: Option<i64>) -> Vec<i64> {
        let unscaled: Vec<i64> = self
            .max_widths
            .iter()
            .map(|w| (w + padding) as i64)
            .collect();
        let Some(max_width) = max_width else {
            return unscaled;
        };
        if unscaled.is_empty() {
            return unscaled;
        }
        let total: i64 = unscaled.iter().sum();
        let scale_factor = max_width as f64 / total as f64;
        let mut scaled: Vec<i64> = unscaled
            .iter()
            .map(|&w| (scale_factor * w as f64).round_ties_even() as i64)
            .collect();
        let mut off_by: i64 = scaled.iter().sum::<i64>() - max_width;
        while off_by != 0 {
            let order: Vec<usize> = if off_by < 0 {
                (0..scaled.len()).rev().collect()
            } else {
                (0..scaled.len()).collect()
            };
            for i in order {
                if off_by > 0 {
                    scaled[i] -= 1;
                    off_by -= 1;
                } else {
                    scaled[i] += 1;
                    off_by += 1;
                }
                if off_by == 0 {
                    break;
                }
            }
        }
        scaled
    }

    /// `Section.total_width`.
    pub fn total_width(&self, padding: usize, with_border: bool, outer_padding: usize) -> i64 {
        let border_padding = 2;
        let mut total: i64 = 0;
        for w in self.calculate_column_widths(0, None) {
            total += w + padding as i64;
        }
        if with_border {
            total += border_padding;
        }
        total += (outer_padding + outer_padding) as i64;
        let title = get_text_length(if self.title_is_none {
            "None"
        } else {
            &self.title
        }) as i64
            + border_padding
            + (outer_padding + outer_padding) as i64;
        title.max(total)
    }

    /// `Section.add_header`.
    pub fn add_header(&mut self, headers: Vec<String>) -> Result<(), String> {
        self.update_max_widths(&headers)?;
        if self.num_cols.is_none() {
            self.num_cols = Some(headers.len());
        }
        self.headers = headers;
        Ok(())
    }

    /// `Section.add_row` (elements are converted with `str()`).
    pub fn add_row(&mut self, row: Vec<String>) -> Result<(), String> {
        if self.num_cols.is_none() {
            self.num_cols = Some(row.len());
        }
        let num_cols = self.num_cols.unwrap_or_default();
        if row.len() != num_cols {
            return Err(format!(
                "Row should have {} elements, instead it has {}",
                num_cols,
                row.len()
            ));
        }
        self.update_max_widths(&row)?;
        self.rows.push(row);
        Ok(())
    }

    /// `Section._update_max_widths`.
    fn update_max_widths(&mut self, row: &[String]) -> Result<(), String> {
        if self.max_widths.is_empty() {
            self.max_widths = row.iter().map(|el| get_text_length(el)).collect();
        } else {
            for (i, el) in row.iter().enumerate() {
                let Some(w) = self.max_widths.get_mut(i) else {
                    return Err("list index out of range".to_string());
                };
                *w = (*w).max(get_text_length(el));
            }
        }
        Ok(())
    }
}

/// `table.convert_to_vertical_table`.
fn convert_to_vertical_table(sections: &mut [Section]) -> Result<(), String> {
    for section in sections.iter_mut() {
        if section.rows.len() == 1 && !section.headers.is_empty() {
            let mut new_section = Section {
                title: section.title.clone(),
                title_is_none: section.title_is_none,
                indent_level: section.indent_level,
                ..Section::default()
            };
            for (header, element) in section.headers.iter().zip(section.rows[0].iter()) {
                new_section.add_row(vec![header.clone(), element.clone()])?;
            }
            *section = new_section;
        }
    }
    Ok(())
}

/// `table.MultiTable` (created with `initial_section=False`,
/// `column_separator='|'`, `auto_reformat=True`).
pub struct MultiTable {
    sections: Vec<Section>,
    current: Option<usize>,
    styler: Styler,
    terminal_width: i64,
    column_separator: &'static str,
}

impl MultiTable {
    pub fn new(styler: Styler, terminal_width: usize) -> MultiTable {
        MultiTable {
            sections: Vec::new(),
            current: None,
            styler,
            terminal_width: terminal_width as i64,
            column_separator: "|",
        }
    }

    fn current(&mut self) -> Result<&mut Section, String> {
        match self.current {
            Some(i) => Ok(&mut self.sections[i]),
            None => Err("'NoneType' object has no attribute 'add_row'".to_string()),
        }
    }

    pub fn add_row_header(&mut self, headers: Vec<String>) -> Result<(), String> {
        self.current()?.add_header(headers)
    }

    pub fn add_row(&mut self, row: Vec<String>) -> Result<(), String> {
        self.current()?.add_row(row)
    }

    pub fn new_section(&mut self, title: &str, indent_level: usize) {
        self.sections.push(Section {
            title: title.to_string(),
            indent_level,
            ..Section::default()
        });
        self.current = Some(self.sections.len() - 1);
    }

    /// `new_section(None, indent_level)`.
    pub fn new_section_none(&mut self, indent_level: usize) {
        self.new_section("", indent_level);
        if let Some(section) = self.sections.last_mut() {
            section.title_is_none = true;
        }
    }

    /// `MultiTable.render`.
    pub fn render(&mut self, stream: &mut Stream<'_>) -> Result<(), String> {
        let mut max_width = self.calculate_max_width()?;
        if max_width > self.terminal_width {
            convert_to_vertical_table(&mut self.sections)?;
            max_width = self.calculate_max_width()?;
        }
        stream.write(&format!("{}\n", "-".repeat(max_width.max(0) as usize)));
        for section in &self.sections {
            self.render_section(section, max_width, stream);
        }
        Ok(())
    }

    fn calculate_max_width(&self) -> Result<i64, String> {
        self.sections
            .iter()
            .map(|s| s.total_width(4, true, s.indent_level))
            .max()
            .ok_or_else(|| "max() iterable argument is empty".to_string())
    }

    fn render_section(&self, section: &Section, max_width: i64, stream: &mut Stream<'_>) {
        let indent = self.styler.style_indentation_char("|");
        let mut ctx = SectionWriter {
            stream,
            indent_level: section.indent_level,
            left: &indent,
            right: &indent,
        };
        let max_width = max_width - (section.indent_level * 2) as i64;
        self.render_title(section, max_width, &mut ctx);
        self.render_column_titles(section, max_width, &mut ctx);
        self.render_rows(section, max_width, &mut ctx);
    }

    fn render_title(&self, section: &Section, max_width: i64, ctx: &mut SectionWriter<'_, '_>) {
        if !section.title.is_empty() {
            let title = self.styler.style_title(&section.title);
            ctx.write(&format!(
                "{}\n",
                center_text(
                    &title,
                    max_width,
                    "|",
                    "|",
                    Some(get_text_length(&section.title))
                )
            ));
            if section.headers.is_empty() && section.rows.is_empty() {
                ctx.write(&format!(
                    "+{}+\n",
                    "-".repeat((max_width - 2).max(0) as usize)
                ));
            }
        }
    }

    fn render_column_titles(
        &self,
        section: &Section,
        max_width: i64,
        ctx: &mut SectionWriter<'_, '_>,
    ) {
        if section.headers.is_empty() {
            return;
        }
        let widths = section.calculate_column_widths(4, Some(max_width));
        let mut current = String::new();
        let mut first = true;
        for (width, header) in widths.iter().zip(section.headers.iter()) {
            let stylized = self.styler.style_header_column(header);
            let left_edge = if first { "|" } else { "" };
            first = false;
            current.push_str(&center_text(
                &stylized,
                *width,
                left_edge,
                "|",
                Some(get_text_length(header)),
            ));
        }
        write_line_break(ctx, &widths);
        ctx.write(&format!("{current}\n"));
    }

    fn render_rows(&self, section: &Section, max_width: i64, ctx: &mut SectionWriter<'_, '_>) {
        if section.rows.is_empty() {
            return;
        }
        let widths = section.calculate_column_widths(4, Some(max_width));
        if widths.is_empty() {
            return;
        }
        write_line_break(ctx, &widths);
        for row in &section.rows {
            let mut current = String::new();
            let mut first = true;
            for (width, element) in widths.iter().zip(row.iter()) {
                let left_edge = if first { "|" } else { "" };
                first = false;
                let stylized = self.styler.style_row_element(element);
                current.push_str(&align_left(
                    &stylized,
                    *width,
                    left_edge,
                    self.column_separator,
                    Some(get_text_length(element)),
                ));
            }
            ctx.write(&format!("{current}\n"));
        }
        write_line_break(ctx, &widths);
    }
}

struct SectionWriter<'s, 'o> {
    stream: &'s mut Stream<'o>,
    indent_level: usize,
    left: &'s str,
    right: &'s str,
}

impl SectionWriter<'_, '_> {
    fn write(&mut self, text: &str) {
        indented_write(self.stream, self.indent_level, self.left, self.right, text);
    }
}

/// `MultiTable._write_line_break`.
fn write_line_break(ctx: &mut SectionWriter<'_, '_>, widths: &[i64]) {
    let mut parts = String::new();
    for (i, width) in widths.iter().enumerate() {
        if i == 0 {
            parts.push_str(&format!("+{}+", "-".repeat((width - 2).max(0) as usize)));
        } else {
            parts.push_str(&format!("{}+", "-".repeat((width - 1).max(0) as usize)));
        }
    }
    parts.push('\n');
    ctx.write(&parts);
}

/// `TableFormatter._build_table` and its helpers.
pub struct TableBuilder {
    pub table: MultiTable,
}

fn is_scalar(v: &Value) -> bool {
    v.is_scalar()
}

impl TableBuilder {
    /// `TableFormatter._build_table`.
    pub fn build_table(
        &mut self,
        title: Option<&str>,
        current: &Value,
        indent_level: usize,
    ) -> Result<bool, String> {
        if !current.is_truthy() {
            return Ok(false);
        }
        if let Some(title) = title {
            self.table.new_section(title, indent_level);
        }
        if let Value::List(list) = current {
            if matches!(list[0], Value::Map(_)) {
                self.build_sub_table_from_list(list, indent_level, title)?;
            } else {
                for item in list {
                    match item {
                        _ if is_scalar(item) => self.table.add_row(vec![py_str(item)])?,
                        Value::List(l) if l.iter().all(is_scalar) => {
                            self.table.add_row(l.iter().map(py_str).collect())?
                        }
                        // Iterating a dict yields its keys, which are all
                        // scalars.
                        Value::Map(m) => self.table.add_row(m.keys().cloned().collect())?,
                        _ => {
                            self.build_table(None, item, 0)?;
                        }
                    }
                }
            }
        }
        if let Value::Map(_) = current {
            self.build_sub_table_from_dict(current, indent_level)?;
        }
        Ok(true)
    }

    /// `TableFormatter._build_sub_table_from_dict`.
    fn build_sub_table_from_dict(
        &mut self,
        current: &Value,
        indent_level: usize,
    ) -> Result<(), String> {
        let Value::Map(map) = current else {
            return Ok(());
        };
        let (headers, more) = group_scalar_keys(current)?;
        if headers.len() == 1 {
            self.table
                .add_row(vec![headers[0].clone(), py_str(&map[headers[0].as_str()])])?;
        } else if !headers.is_empty() {
            self.table.add_row_header(headers.clone())?;
            self.table
                .add_row(headers.iter().map(|k| py_str(&map[k.as_str()])).collect())?;
        }
        for remaining in &more {
            self.build_table(Some(remaining), &map[remaining.as_str()], indent_level + 1)?;
        }
        Ok(())
    }

    /// `TableFormatter._build_sub_table_from_list`.
    fn build_sub_table_from_list(
        &mut self,
        current: &[Value],
        indent_level: usize,
        title: Option<&str>,
    ) -> Result<(), String> {
        let (headers, more) = group_scalar_keys_from_list(current)?;
        self.table.add_row_header(headers.clone())?;
        let mut first = true;
        for element in current {
            if !first && !more.is_empty() {
                match title {
                    Some(title) => self.table.new_section(title, indent_level),
                    None => self.table.new_section_none(indent_level),
                }
                self.table.add_row_header(headers.clone())?;
            }
            first = false;
            let Value::Map(map) = element else {
                return Err(format!(
                    "'{}' object has no attribute 'get'",
                    type_name(element)
                ));
            };
            self.table.add_row(
                headers
                    .iter()
                    .map(|h| map.get(h.as_str()).map(py_str).unwrap_or_default())
                    .collect(),
            )?;
            for remaining in &more {
                if let Some(v) = map.get(remaining.as_str()) {
                    self.build_table(Some(remaining), v, indent_level + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// `TableFormatter._group_scalar_keys_from_list`.
fn group_scalar_keys_from_list(
    list_of_dicts: &[Value],
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut headers = std::collections::BTreeSet::new();
    let mut more = std::collections::BTreeSet::new();
    for item in list_of_dicts {
        let (h, m) = group_scalar_keys(item)?;
        headers.extend(h);
        more.extend(m);
    }
    Ok((headers.into_iter().collect(), more.into_iter().collect()))
}

/// `TableFormatter._group_scalar_keys`.
fn group_scalar_keys(current: &Value) -> Result<(Vec<String>, Vec<String>), String> {
    let Value::Map(map) = current else {
        return Err(match current {
            Value::List(_) => "list indices must be integers or slices, not str".to_string(),
            Value::Str(_) => "string indices must be integers, not 'str'".to_string(),
            other => format!("'{}' object is not iterable", type_name(other)),
        });
    };
    let mut headers = Vec::new();
    let mut more = Vec::new();
    for (k, v) in map {
        if is_scalar(v) {
            headers.push(k.clone());
        } else {
            more.push(k.clone());
        }
    }
    headers.sort();
    more.sort();
    Ok((headers, more))
}
