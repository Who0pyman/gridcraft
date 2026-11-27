use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read};

use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_model::{Style, Workbook};

use crate::package::{Package, ZipOut, normalize};
use crate::{IoError, xml};

struct PatchStyles {
    part: Option<String>,
    text: Option<String>,
    style_to_xf: HashMap<Style, u32>,
    next_xf: u32,
    next_font: u32,
    next_fill: u32,
    next_border: u32,
    num_fmt_count: u32,
    custom_num_fmts: HashMap<String, u32>,
    used_num_fmt_ids: BTreeSet<u32>,
    changed: bool,
}

impl PatchStyles {
    fn load(pkg: &mut Package<'_>, part: Option<String>) -> Result<Self, IoError> {
        let Some(name) = &part else {
            return Ok(Self {
                part,
                text: None,
                style_to_xf: HashMap::new(),
                next_xf: 0,
                next_font: 0,
                next_fill: 0,
                next_border: 0,
                num_fmt_count: 0,
                custom_num_fmts: HashMap::new(),
                used_num_fmt_ids: BTreeSet::new(),
                changed: false,
            });
        };

        let bytes = pkg.read(name)?.ok_or_else(|| IoError::Format(format!("missing styles part {name}")))?;
        let text = String::from_utf8(bytes.clone()).map_err(|_| IoError::Xml("styles.xml is not UTF-8".into()))?;
        let root = xml::parse(&bytes)?;
        let parsed = crate::styles::read_styles(&root);

        let mut style_to_xf = HashMap::new();
        for (index, style) in parsed.xfs.iter().cloned().enumerate() {
            style_to_xf.entry(style).or_insert(index as u32);
        }

        let next_font = root.child("fonts").map(|e| e.kids("font").count() as u32).unwrap_or(0);
        let next_fill = root.child("fills").map(|e| e.kids("fill").count() as u32).unwrap_or(0);
        let next_border = root.child("borders").map(|e| e.kids("border").count() as u32).unwrap_or(0);
        let next_xf = root.child("cellXfs").map(|e| e.kids("xf").count() as u32).unwrap_or(0);
        let num_fmt_count = root.child("numFmts").map(|e| e.kids("numFmt").count() as u32).unwrap_or(0);

        let mut custom_num_fmts = HashMap::new();
        let mut used_num_fmt_ids = BTreeSet::new();

        for (id, code) in parsed.num_fmts {
            custom_num_fmts.entry(code).or_insert(id);
            used_num_fmt_ids.insert(id);
        }

        for table in ["cellXfs", "cellStyleXfs"] {
            if let Some(container) = root.child(table) {
                for xf in container.kids("xf") {
                    if let Some(id) = xf.attr_u32("numFmtId") {
                        used_num_fmt_ids.insert(id);
                    }
                }
            }
        }

        Ok(Self {
            part,
            text: Some(text),
            style_to_xf,
            next_xf,
            next_font,
            next_fill,
            next_border,
            num_fmt_count,
            custom_num_fmts,
            used_num_fmt_ids,
            changed: false,
        })
    }

    fn style_index(&mut self, style: &Style) -> Result<u32, IoError> {
        if let Some(&index) = self.style_to_xf.get(style) {
            return Ok(index);
        }

        if self.text.is_none() {
            return Err(IoError::Format("cannot create a non-default style because the original workbook has no styles part".into()));
        }

        let font_id = self.next_font;
        self.next_font = self.next_font.saturating_add(1);
        append_style_table_entry(
            self.text.as_mut().ok_or_else(|| IoError::Format("styles.xml text unavailable during safe style patching".into()))?,
            "fonts",
            &crate::styles::font_xml(&style.font, false),
            self.next_font,
        )?;

        let fill_id = self.next_fill;
        self.next_fill = self.next_fill.saturating_add(1);
        append_style_table_entry(
            self.text.as_mut().ok_or_else(|| IoError::Format("styles.xml text unavailable during safe style patching".into()))?,
            "fills",
            &crate::styles::fill_xml(&style.fill, false),
            self.next_fill,
        )?;

        let border_id = self.next_border;
        self.next_border = self.next_border.saturating_add(1);
        append_style_table_entry(
            self.text.as_mut().ok_or_else(|| IoError::Format("styles.xml text unavailable during safe style patching".into()))?,
            "borders",
            &crate::styles::border_xml(&style.borders),
            self.next_border,
        )?;

        let num_fmt_id = if let Some(id) = crate::tables::builtin_id(style.num_fmt.as_str()) {
            id
        } else if let Some(&id) = self.custom_num_fmts.get(style.num_fmt.as_str()) {
            id
        } else {
            let mut id = 164u32;
            while self.used_num_fmt_ids.contains(&id) {
                id = id.saturating_add(1);
            }

            self.used_num_fmt_ids.insert(id);
            self.custom_num_fmts.insert(style.num_fmt.as_str().to_string(), id);

            let entry = format!(r#"<numFmt numFmtId="{id}" formatCode="{}"/>"#, xml::esc_attr(style.num_fmt.as_str()));

            append_num_fmt_entry(
                self.text.as_mut().ok_or_else(|| IoError::Format("styles.xml text unavailable during safe style patching".into()))?,
                &entry,
                self.num_fmt_count,
            )?;
            self.num_fmt_count = self.num_fmt_count.saturating_add(1);
            id
        };

        let alignment = crate::styles::alignment_xml(&style.align);
        let protection = crate::styles::protection_xml(&style.protection);

        let mut xf = format!(r#"<xf numFmtId="{num_fmt_id}" fontId="{font_id}" fillId="{fill_id}" borderId="{border_id}" xfId="0""#);

        if num_fmt_id != 0 {
            xf.push_str(r#" applyNumberFormat="1""#);
        }
        if font_id != 0 {
            xf.push_str(r#" applyFont="1""#);
        }
        if fill_id != 0 {
            xf.push_str(r#" applyFill="1""#);
        }
        if border_id != 0 {
            xf.push_str(r#" applyBorder="1""#);
        }
        if alignment.is_some() {
            xf.push_str(r#" applyAlignment="1""#);
        }
        if protection.is_some() {
            xf.push_str(r#" applyProtection="1""#);
        }

        if alignment.is_none() && protection.is_none() {
            xf.push_str("/>");
        } else {
            xf.push('>');
            xf.push_str(alignment.as_deref().unwrap_or(""));
            xf.push_str(protection.as_deref().unwrap_or(""));
            xf.push_str("</xf>");
        }

        let index = self.next_xf;
        self.next_xf = self.next_xf.saturating_add(1);
        append_style_table_entry(
            self.text.as_mut().ok_or_else(|| IoError::Format("styles.xml text unavailable during safe style patching".into()))?,
            "cellXfs",
            &xf,
            self.next_xf,
        )?;

        self.style_to_xf.insert(style.clone(), index);
        self.changed = true;

        Ok(index)
    }

    fn replacement(&self) -> Option<(&str, Vec<u8>)> {
        if !self.changed {
            return None;
        }

        Some((self.part.as_deref()?, self.text.as_ref()?.as_bytes().to_vec()))
    }
}

fn append_style_table_entry(text: &mut String, tag: &str, entry: &str, new_count: u32) -> Result<(), IoError> {
    let open_pat = format!("<{tag}");
    let start = text.find(&open_pat).ok_or_else(|| IoError::Xml(format!("styles.xml has no <{tag}> element")))?;

    let open_end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml(format!("styles.xml has an unterminated <{tag}> element")))?;
    let open_end = start + open_end_rel;

    let original = text[start..=open_end].to_string();

    if original.trim_end().ends_with("/>") {
        return Err(IoError::Format(format!("safe XLSX patching does not yet support self-closing <{tag}/> style tables")));
    }

    let mut open = original[..original.len() - 1].trim_end().to_string();
    open = remove_attr(&open, "count");
    open.push_str(&format!(r#" count="{new_count}">"#));

    text.replace_range(start..=open_end, &open);

    let close_pat = format!("</{tag}>");
    let close = text[start + open.len()..]
        .find(&close_pat)
        .map(|offset| start + open.len() + offset)
        .ok_or_else(|| IoError::Xml(format!("styles.xml has no {close_pat}")))?;

    text.insert_str(close, entry);
    Ok(())
}

fn append_num_fmt_entry(text: &mut String, entry: &str, old_count: u32) -> Result<(), IoError> {
    if text.contains("<numFmts") {
        return append_style_table_entry(text, "numFmts", entry, old_count.saturating_add(1));
    }

    let fonts = text.find("<fonts").ok_or_else(|| IoError::Xml("styles.xml has no <fonts> element".into()))?;

    text.insert_str(fonts, &format!(r#"<numFmts count="1">{entry}</numFmts>"#));
    Ok(())
}

struct PatchSst {
    part: Option<String>,
    map: HashMap<String, u32>,
    next_index: u32,
    new_strings: Vec<String>,
    count_delta: i64,
}

impl PatchSst {
    fn load(pkg: &mut Package<'_>, part: Option<String>) -> Result<Self, IoError> {
        let mut map = HashMap::new();
        let mut index = 0u32;

        if let Some(name) = &part
            && let Some(bytes) = pkg.read(name)?
        {
            xml::parse_streaming(&bytes, &["si"], &mut |si| {
                let text = xml::rich_text(&si);
                map.entry(text).or_insert(index);
                index = index.saturating_add(1);
                Ok(())
            })?;
        }

        Ok(Self { part, map, next_index: index, new_strings: Vec::new(), count_delta: 0 })
    }

    fn index(&mut self, text: &str) -> Result<u32, IoError> {
        if let Some(&index) = self.map.get(text) {
            return Ok(index);
        }

        if self.part.is_none() {
            return Err(IoError::Format("cannot add a shared string because the original workbook has no sharedStrings part".into()));
        }

        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        self.map.insert(text.to_string(), index);
        self.new_strings.push(text.to_string());
        Ok(index)
    }

    fn changed(&self) -> bool {
        self.count_delta != 0 || !self.new_strings.is_empty()
    }
}

fn shifted_cell_ref(cell: CellRef, edit: crate::StructuralEdit) -> Option<CellRef> {
    match edit {
        crate::StructuralEdit::InsertRows { at, count, .. } => {
            if cell.row >= at {
                Some(CellRef::new(cell.row.checked_add(count)?, cell.col))
            } else {
                Some(cell)
            }
        }
        crate::StructuralEdit::DeleteRows { at, count, .. } => {
            let end = at.saturating_add(count);
            if cell.row >= at && cell.row < end {
                None
            } else if cell.row >= end {
                Some(CellRef::new(cell.row - count, cell.col))
            } else {
                Some(cell)
            }
        }
        crate::StructuralEdit::InsertColumns { at, count, .. } => {
            if cell.col >= at {
                Some(CellRef::new(cell.row, cell.col.checked_add(count)?))
            } else {
                Some(cell)
            }
        }
        crate::StructuralEdit::DeleteColumns { at, count, .. } => {
            let end = at.saturating_add(count);
            if cell.col >= at && cell.col < end {
                None
            } else if cell.col >= end {
                Some(CellRef::new(cell.row, cell.col - count))
            } else {
                Some(cell)
            }
        }
        crate::StructuralEdit::InsertCellsRight { .. }
        | crate::StructuralEdit::DeleteCellsLeft { .. }
        | crate::StructuralEdit::InsertCellsDown { .. }
        | crate::StructuralEdit::DeleteCellsUp { .. }
        | crate::StructuralEdit::RenameSheet { .. }
        | crate::StructuralEdit::MoveSheet { .. } => Some(cell),
    }
}

fn set_tag_attr(tag: &str, name: &str, value: &str) -> Result<String, IoError> {
    if !tag.ends_with('>') {
        return Err(IoError::Xml("XML start tag is not closed".into()));
    }

    let self_closing = tag[..tag.len() - 1].trim_end().ends_with('/');
    let mut open = tag[..tag.len() - 1].trim_end().to_string();

    if open.ends_with('/') {
        open.pop();
        open = open.trim_end().to_string();
    }

    open = remove_attr(&open, name);
    open.push_str(&format!(r#" {name}="{}""#, xml::esc_attr(value)));

    if self_closing {
        open.push_str("/>");
    } else {
        open.push('>');
    }

    Ok(open)
}

fn shift_sheet_data_coordinates(xml: &str, edit: crate::StructuralEdit) -> Result<String, IoError> {
    let sheet_data_start = xml.find("<sheetData").ok_or_else(|| IoError::Xml("worksheet has no <sheetData> element".into()))?;
    let sheet_data_tag_end_rel =
        xml[sheet_data_start..].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <sheetData> element".into()))?;
    let sheet_data_tag_end = sheet_data_start + sheet_data_tag_end_rel + 1;
    let sheet_data_start_tag = &xml[sheet_data_start..sheet_data_tag_end];

    if sheet_data_start_tag.trim_end().ends_with("/>") {
        return Ok(xml.to_string());
    }

    let sheet_data_close_rel =
        xml[sheet_data_tag_end..].find("</sheetData>").ok_or_else(|| IoError::Xml("worksheet has no </sheetData> element".into()))?;
    let sheet_data_close = sheet_data_tag_end + sheet_data_close_rel;

    let row_edit = matches!(edit, crate::StructuralEdit::InsertRows { .. } | crate::StructuralEdit::DeleteRows { .. });

    let bytes = xml.as_bytes();
    let mut body = String::with_capacity(sheet_data_close - sheet_data_tag_end);
    let mut pos = sheet_data_tag_end;

    while pos < sheet_data_close {
        let Some(rel) = xml[pos..sheet_data_close].find("<row") else {
            body.push_str(&xml[pos..sheet_data_close]);
            break;
        };

        let row_start = pos + rel;
        let after_row = row_start + 4;
        let next = bytes.get(after_row).copied();

        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            body.push_str(&xml[pos..after_row]);
            pos = after_row;
            continue;
        }

        body.push_str(&xml[pos..row_start]);

        let row_tag_end_rel =
            xml[row_start..sheet_data_close].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <row> element".into()))?;
        let row_tag_end = row_start + row_tag_end_rel + 1;
        let row_start_tag = &xml[row_start..row_tag_end];
        let row_self_closing = row_start_tag.trim_end().ends_with("/>");

        let row_end = if row_self_closing {
            row_tag_end
        } else {
            let close_rel =
                xml[row_tag_end..sheet_data_close].find("</row>").ok_or_else(|| IoError::Xml("worksheet row has no </row> element".into()))?;
            row_tag_end + close_rel + 6
        };

        let shifted_row = if row_edit {
            let row_number = attr_value(row_start_tag, "r")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| IoError::Format("safe structural XLSX patching requires explicit row numbers".into()))?;

            let row_zero = row_number.checked_sub(1).ok_or_else(|| IoError::Format("worksheet row number cannot be zero".into()))?;

            shifted_cell_ref(CellRef::new(row_zero, 0), edit).map(|c| c.row.saturating_add(1))
        } else {
            attr_value(row_start_tag, "r").and_then(|value| value.parse::<u32>().ok())
        };

        if row_edit && shifted_row.is_none() {
            pos = row_end;
            continue;
        }

        let new_row_start = if row_edit {
            let Some(row) = shifted_row else {
                return Err(IoError::Format("safe structural XLSX patching could not resolve shifted row".into()));
            };
            set_tag_attr(row_start_tag, "r", &row.to_string())?
        } else {
            row_start_tag.to_string()
        };

        body.push_str(&new_row_start);

        if !row_self_closing {
            let row_close = row_end - 6;
            let mut cell_pos = row_tag_end;

            while cell_pos < row_close {
                let Some(cell_rel) = xml[cell_pos..row_close].find("<c") else {
                    body.push_str(&xml[cell_pos..row_close]);
                    break;
                };

                let cell_start = cell_pos + cell_rel;
                let after_c = cell_start + 2;
                let next = bytes.get(after_c).copied();

                if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                    body.push_str(&xml[cell_pos..after_c]);
                    cell_pos = after_c;
                    continue;
                }

                body.push_str(&xml[cell_pos..cell_start]);

                let cell_tag_end_rel =
                    xml[cell_start..row_close].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <c> element".into()))?;
                let cell_tag_end = cell_start + cell_tag_end_rel + 1;
                let cell_start_tag = &xml[cell_start..cell_tag_end];

                let cell_end = if cell_start_tag.trim_end().ends_with("/>") {
                    cell_tag_end
                } else {
                    let close_rel =
                        xml[cell_tag_end..row_close].find("</c>").ok_or_else(|| IoError::Xml("worksheet cell has no </c> element".into()))?;
                    cell_tag_end + close_rel + 4
                };

                let a1 = attr_value(cell_start_tag, "r")
                    .ok_or_else(|| IoError::Format("safe structural XLSX patching requires explicit cell references".into()))?;
                let cell_ref = CellRef::parse(&a1).ok_or_else(|| IoError::Format(format!("invalid worksheet cell reference {a1}")))?;

                if let Some(new_ref) = shifted_cell_ref(cell_ref, edit) {
                    let new_start = set_tag_attr(cell_start_tag, "r", &new_ref.a1())?;
                    body.push_str(&new_start);
                    body.push_str(&xml[cell_tag_end..cell_end]);
                }

                cell_pos = cell_end;
            }

            body.push_str("</row>");
        }

        pos = row_end;
    }

    let mut out = String::with_capacity(xml.len() + 64);
    out.push_str(&xml[..sheet_data_tag_end]);
    out.push_str(&body);
    out.push_str(&xml[sheet_data_close..]);
    Ok(out)
}

fn structural_sheet_is_plain(sheet: &gridcraft_model::Sheet) -> bool {
    sheet.rows.is_empty()
        && sheet.cols.is_empty()
        && sheet.merges.is_empty()
        && sheet.cond_formats.is_empty()
        && sheet.validations.is_empty()
        && sheet.tables.is_empty()
        && sheet.autofilter.is_none()
        && sheet.cell_pictures.is_empty()
        && sheet.comments.is_empty()
        && sheet.hyperlinks.is_empty()
        && sheet.charts.is_empty()
        && sheet.images.is_empty()
        && sheet.shapes.is_empty()
        && sheet.sparklines.is_empty()
        && sheet.pivots.is_empty()
}

fn partial_cell_edit_affects_range(edit: crate::StructuralEdit, range: gridcraft_core::RangeRef) -> bool {
    match edit {
        crate::StructuralEdit::InsertCellsRight { start_row, end_row, at_col, .. }
        | crate::StructuralEdit::DeleteCellsLeft { start_row, end_row, at_col, .. } => {
            range.start.row <= end_row && range.end.row >= start_row && range.end.col >= at_col
        }
        crate::StructuralEdit::InsertCellsDown { start_col, end_col, at_row, .. }
        | crate::StructuralEdit::DeleteCellsUp { start_col, end_col, at_row, .. } => {
            range.start.col <= end_col && range.end.col >= start_col && range.end.row >= at_row
        }
        _ => false,
    }
}

fn partial_cell_sheet_is_plain(sheet: &gridcraft_model::Sheet, edit: crate::StructuralEdit) -> bool {
    let range_affected = |range| partial_cell_edit_affects_range(edit, range);
    let cell_affected = |cell| range_affected(gridcraft_core::RangeRef::cell(cell));

    !sheet.merges.iter().any(|&range| range_affected(range))
        && !sheet.cond_formats.iter().any(|cf| cf.ranges.iter().any(|&range| range_affected(range)))
        && !sheet.validations.iter().any(|dv| dv.ranges.iter().any(|&range| range_affected(range)))
        && !sheet.tables.iter().any(|table| range_affected(table.range))
        && !sheet.autofilter.as_ref().is_some_and(|af| range_affected(af.range))
        && !sheet.cell_pictures.keys().any(|&cell| cell_affected(cell))
        && !sheet.comments.keys().any(|&cell| cell_affected(cell))
        && !sheet.hyperlinks.keys().any(|&cell| cell_affected(cell))
        && !sheet.sparklines.iter().any(|sp| cell_affected(sp.cell))
        && !sheet.print.print_area.is_some_and(range_affected)
}

fn apply_structural_edit_to_baseline(wb: &mut Workbook, edit: crate::StructuralEdit) -> Result<(), IoError> {
    if let crate::StructuralEdit::MoveSheet { from, to } = edit {
        if from >= wb.sheets.len() {
            return Err(IoError::Format(format!("sheet move refers to missing source sheet {from}")));
        }

        let sheet = wb.sheets.remove(from);
        let at = to.min(wb.sheets.len());
        wb.sheets.insert(at, sheet);
        wb.active_sheet = at;

        for name in &mut wb.names {
            if let Some(scope) = name.scope.as_mut() {
                *scope = match *scope {
                    x if x == from => at,
                    x if from < x && x <= at => x - 1,
                    x if at <= x && x < from => x + 1,
                    x => x,
                };
            }
        }

        return Ok(());
    }

    let sheet_index = match edit {
        crate::StructuralEdit::InsertRows { sheet, .. }
        | crate::StructuralEdit::DeleteRows { sheet, .. }
        | crate::StructuralEdit::InsertColumns { sheet, .. }
        | crate::StructuralEdit::DeleteColumns { sheet, .. }
        | crate::StructuralEdit::InsertCellsRight { sheet, .. }
        | crate::StructuralEdit::DeleteCellsLeft { sheet, .. }
        | crate::StructuralEdit::InsertCellsDown { sheet, .. }
        | crate::StructuralEdit::DeleteCellsUp { sheet, .. }
        | crate::StructuralEdit::RenameSheet { sheet } => sheet,
        crate::StructuralEdit::MoveSheet { .. } => {
            return Err(IoError::Format("sheet move should have been handled before cell edits".into()));
        }
    };

    let sheet = wb.sheet_mut(sheet_index).ok_or_else(|| IoError::Format(format!("structural edit refers to missing sheet {sheet_index}")))?;

    match edit {
        crate::StructuralEdit::InsertRows { at, count, .. } => {
            if count != 0 {
                sheet.cells.shift_rows(at, count as i64);
            }
        }
        crate::StructuralEdit::DeleteRows { at, count, .. } => {
            if count != 0 {
                let end = at.saturating_add(count).saturating_sub(1).min(gridcraft_core::MAX_ROWS - 1);
                sheet.cells.take_range(gridcraft_core::RangeRef::rows(at, end));
                sheet.cells.shift_rows(at.saturating_add(count), -(count as i64));
            }
        }
        crate::StructuralEdit::InsertColumns { at, count, .. } => {
            if count != 0 {
                sheet.cells.shift_cols(at, count as i64);
            }
        }
        crate::StructuralEdit::DeleteColumns { at, count, .. } => {
            if count != 0 {
                let end = at.saturating_add(count).saturating_sub(1).min(gridcraft_core::MAX_COLS - 1);
                sheet.cells.take_range(gridcraft_core::RangeRef::cols(at, end));
                sheet.cells.shift_cols(at.saturating_add(count), -(count as i64));
            }
        }
        // Partial-cell shifts are intentionally NOT applied to the baseline here.
        // Their package XML is likewise left in place, so the final model diff
        // patches every changed source/destination cell against the original layout.
        crate::StructuralEdit::InsertCellsRight { .. }
        | crate::StructuralEdit::DeleteCellsLeft { .. }
        | crate::StructuralEdit::InsertCellsDown { .. }
        | crate::StructuralEdit::DeleteCellsUp { .. }
        | crate::StructuralEdit::RenameSheet { .. } => {}
        crate::StructuralEdit::MoveSheet { .. } => {
            return Err(IoError::Format("sheet move should have been handled before cell edits".into()));
        }
    }

    Ok(())
}

fn structurally_shift_package(original: &[u8], base: &Workbook, edits: &[crate::StructuralEdit]) -> Result<Vec<u8>, IoError> {
    if edits.is_empty() {
        return Ok(original.to_vec());
    }

    let mut pkg = Package::open(original)?;

    let root_rels = pkg.rels("")?;
    let wb_part = root_rels.iter().find(|r| r.kind == "officeDocument").map(|r| r.target.clone()).unwrap_or_else(|| "xl/workbook.xml".into());

    let wb_xml = pkg.read_xml(&wb_part)?.ok_or_else(|| IoError::Format("no workbook part".into()))?;
    let wb_rels = pkg.rels(&wb_part)?;

    let mut sheet_parts = Vec::new();
    if let Some(sheets) = wb_xml.child("sheets") {
        for sheet in sheets.kids("sheet") {
            let Some(id) = sheet.attr("id") else {
                continue;
            };
            let Some(rel) = wb_rels.iter().find(|r| r.id == id) else {
                continue;
            };
            if rel.kind == "worksheet" {
                sheet_parts.push(rel.target.clone());
            }
        }
    }

    let mut by_sheet: BTreeMap<usize, Vec<crate::StructuralEdit>> = BTreeMap::new();
    for &edit in edits {
        let sheet_index = match edit {
            crate::StructuralEdit::InsertRows { sheet, .. }
            | crate::StructuralEdit::DeleteRows { sheet, .. }
            | crate::StructuralEdit::InsertColumns { sheet, .. }
            | crate::StructuralEdit::DeleteColumns { sheet, .. }
            | crate::StructuralEdit::InsertCellsRight { sheet, .. }
            | crate::StructuralEdit::DeleteCellsLeft { sheet, .. }
            | crate::StructuralEdit::InsertCellsDown { sheet, .. }
            | crate::StructuralEdit::DeleteCellsUp { sheet, .. }
            | crate::StructuralEdit::RenameSheet { sheet } => sheet,
            crate::StructuralEdit::MoveSheet { .. } => continue,
        };

        let sheet = base.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("structural edit refers to missing base sheet {sheet_index}")))?;

        let safe = match edit {
            crate::StructuralEdit::InsertRows { .. }
            | crate::StructuralEdit::DeleteRows { .. }
            | crate::StructuralEdit::InsertColumns { .. }
            | crate::StructuralEdit::DeleteColumns { .. } => structural_sheet_is_plain(sheet),
            crate::StructuralEdit::InsertCellsRight { .. }
            | crate::StructuralEdit::DeleteCellsLeft { .. }
            | crate::StructuralEdit::InsertCellsDown { .. }
            | crate::StructuralEdit::DeleteCellsUp { .. } => partial_cell_sheet_is_plain(sheet, edit),
            crate::StructuralEdit::RenameSheet { .. } => true,
            crate::StructuralEdit::MoveSheet { .. } => true,
        };

        if !safe {
            return Err(IoError::Format(format!(
                "safe structural XLSX patching does not yet support this edit on sheets with affected structural features: {}",
                sheet.name
            )));
        }

        by_sheet.entry(sheet_index).or_default().push(edit);
    }

    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();

    for (sheet_index, sheet_edits) in by_sheet {
        let part = sheet_parts.get(sheet_index).ok_or_else(|| IoError::Format(format!("cannot resolve worksheet part for sheet {sheet_index}")))?;

        let bytes = pkg.read(part)?.ok_or_else(|| IoError::Format(format!("missing worksheet part {part}")))?;

        let mut text = String::from_utf8(bytes).map_err(|_| IoError::Xml(format!("{part} is not UTF-8 XML")))?;

        for edit in sheet_edits {
            if matches!(
                edit,
                crate::StructuralEdit::InsertRows { .. }
                    | crate::StructuralEdit::DeleteRows { .. }
                    | crate::StructuralEdit::InsertColumns { .. }
                    | crate::StructuralEdit::DeleteColumns { .. }
            ) {
                text = shift_sheet_data_coordinates(&text, edit)?;
            }
        }

        replacements.insert(normalize(part), text.into_bytes());
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| IoError::Zip(e.to_string()))?;
    let mut out = ZipOut::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| IoError::Zip(e.to_string()))?;
        let name = file.name().to_string();

        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes).map_err(|e| IoError::Zip(e.to_string()))?;

        if let Some(replacement) = replacements.get(&normalize(&name)) {
            out.add(&name, replacement)?;
        } else {
            out.add(&name, &bytes)?;
        }
    }

    out.finish()
}

fn rename_formula_for_sheets(text: &str, renames: &[(String, String)]) -> String {
    let Ok(mut expr) = gridcraft_formula::parse(text) else {
        return text.to_string();
    };

    let mut used: BTreeSet<String> = renames.iter().flat_map(|(old, new)| [old.to_ascii_lowercase(), new.to_ascii_lowercase()]).collect();

    let mut staged = Vec::new();
    let mut changed = false;

    for (i, (old, new)) in renames.iter().enumerate() {
        if old == new {
            continue;
        }

        let mut n = i;
        let temp = loop {
            let candidate = format!("__OGRN{n}__");
            if used.insert(candidate.to_ascii_lowercase()) {
                break candidate;
            }
            n += renames.len().max(1);
        };

        let before = gridcraft_formula::print(&expr);
        let next = gridcraft_formula::adjust::rename_sheet(expr, old, &temp);
        let after = gridcraft_formula::print(&next);
        if after != before {
            changed = true;
        }
        expr = next;
        staged.push((temp, new.clone()));
    }

    for (temp, new) in staged {
        expr = gridcraft_formula::adjust::rename_sheet(expr, &temp, &new);
    }

    if changed { gridcraft_formula::print(&expr) } else { text.to_string() }
}

fn patch_workbook_sheet_moves(original: &[u8], base: &Workbook, wb: &Workbook, edits: &[crate::StructuralEdit]) -> Result<Vec<u8>, IoError> {
    let moves: Vec<(usize, usize)> = edits
        .iter()
        .filter_map(|edit| match *edit {
            crate::StructuralEdit::MoveSheet { from, to } => Some((from, to)),
            _ => None,
        })
        .collect();

    if moves.is_empty() {
        return Ok(original.to_vec());
    }

    let mut pkg = Package::open(original)?;
    let root_rels = pkg.rels("")?;
    let wb_part = root_rels.iter().find(|r| r.kind == "officeDocument").map(|r| r.target.clone()).unwrap_or_else(|| "xl/workbook.xml".into());
    let wb_rels = pkg.rels(&wb_part)?;
    let wb_bytes = pkg.read(&wb_part)?.ok_or_else(|| IoError::Format("missing workbook part".into()))?;
    let wb_tree = xml::parse(&wb_bytes)?;
    let mut text = String::from_utf8(wb_bytes).map_err(|_| IoError::Xml(format!("{wb_part} is not UTF-8 XML")))?;

    struct TextEdit {
        start: usize,
        end: usize,
        replacement: String,
    }

    let parsed_sheets: Vec<_> = wb_tree.child("sheets").map(|s| s.kids("sheet").collect()).unwrap_or_default();
    let mut raw_sheet_spans = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find("<sheet") {
        let start = pos + rel;
        let next = text.as_bytes().get(start + "<sheet".len()).copied();
        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            pos = start + "<sheet".len();
            continue;
        }
        let end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <sheet> tag in workbook.xml".into()))?;
        let end = start + end_rel + 1;
        raw_sheet_spans.push((start, end));
        pos = end;
    }

    if raw_sheet_spans.len() != parsed_sheets.len() {
        return Err(IoError::Format("safe sheet move could not align workbook sheet tags".into()));
    }

    let mut worksheet_slots = Vec::new();
    let mut worksheet_tags = Vec::new();

    for (raw_index, ((start, end), sheet_el)) in raw_sheet_spans.iter().copied().zip(parsed_sheets.iter().copied()).enumerate() {
        let is_worksheet = sheet_el.attr("id").and_then(|id| wb_rels.iter().find(|r| r.id == id)).is_some_and(|r| r.kind == "worksheet");
        if is_worksheet {
            worksheet_slots.push(raw_index);
            worksheet_tags.push(text[start..end].to_string());
        }
    }

    if worksheet_tags.len() != base.sheets.len() || worksheet_tags.len() != wb.sheets.len() {
        return Err(IoError::Format("safe sheet move could not align workbook worksheets with the model".into()));
    }

    let mut order: Vec<usize> = (0..worksheet_tags.len()).collect();
    for &(from, to) in &moves {
        if from >= order.len() {
            return Err(IoError::Format(format!("sheet move refers to missing source sheet {from}")));
        }
        let item = order.remove(from);
        let at = to.min(order.len());
        order.insert(at, item);
    }

    let mut text_edits = Vec::new();

    // Keep chart-sheet slots exactly where they were, but reorder the worksheet
    // tags across the worksheet slots to match the model order.
    for (model_index, &raw_index) in worksheet_slots.iter().enumerate() {
        let (start, end) = raw_sheet_spans[raw_index];
        text_edits.push(TextEdit { start, end, replacement: worksheet_tags[order[model_index]].clone() });
    }

    // Map each original raw sheet index to the raw index occupied by that same
    // worksheet after the move. Chart sheets remain fixed in place.
    let mut raw_index_map = BTreeMap::new();
    for (new_model_index, &old_model_index) in order.iter().enumerate() {
        raw_index_map.insert(worksheet_slots[old_model_index], worksheet_slots[new_model_index]);
    }

    // The engine activates the moved worksheet. activeTab is a raw workbook
    // sheet index, not a model worksheet index.
    if wb.active_sheet < worksheet_slots.len()
        && let Some(rel) = text.find("<workbookView")
    {
        let start = rel;
        let next = text.as_bytes().get(start + "<workbookView".len()).copied();
        if matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            let end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <workbookView> tag".into()))?;
            let end = start + end_rel + 1;
            let mut replacement = text[start..end].to_string();

            // firstSheet is also a raw workbook sheet index. If present,
            // keep the leftmost visible tab attached to the same sheet.
            if let Some(old_raw) = attr_value(&replacement, "firstSheet").and_then(|value| value.parse::<usize>().ok())
                && let Some(&new_raw) = raw_index_map.get(&old_raw)
                && new_raw != old_raw
            {
                replacement = set_tag_attr(&replacement, "firstSheet", &new_raw.to_string())?;
            }

            replacement = set_tag_attr(&replacement, "activeTab", &worksheet_slots[wb.active_sheet].to_string())?;
            text_edits.push(TextEdit { start, end, replacement });
        }
    }

    // Sheet-scoped defined names use raw sheet indices. Make the scope follow
    // the same worksheet identity after the reorder.
    let parsed_names: Vec<_> = wb_tree.child("definedNames").map(|d| d.kids("definedName").collect()).unwrap_or_default();
    if !parsed_names.is_empty() {
        let mut raw_name_tags = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = text[pos..].find("<definedName") {
            let start = pos + rel;
            let next = text.as_bytes().get(start + "<definedName".len()).copied();
            if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                pos = start + "<definedName".len();
                continue;
            }
            let end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <definedName> tag".into()))?;
            let end = start + end_rel + 1;
            raw_name_tags.push((start, end));
            pos = end;
        }

        if raw_name_tags.len() != parsed_names.len() {
            return Err(IoError::Format("safe sheet move could not align workbook defined names".into()));
        }

        for ((start, end), parsed) in raw_name_tags.into_iter().zip(parsed_names) {
            let Some(old_raw) = parsed.attr_u32("localSheetId").map(|v| v as usize) else {
                continue;
            };
            let Some(&new_raw) = raw_index_map.get(&old_raw) else {
                continue;
            };
            if new_raw != old_raw {
                let replacement = set_tag_attr(&text[start..end], "localSheetId", &new_raw.to_string())?;
                text_edits.push(TextEdit { start, end, replacement });
            }
        }
    }

    // Custom workbook views also store a raw sheet index. Preserve the sheet
    // each custom view points at when possible.
    let parsed_custom_views: Vec<_> = wb_tree.child("customWorkbookViews").map(|v| v.kids("customWorkbookView").collect()).unwrap_or_default();
    if !parsed_custom_views.is_empty() {
        let mut raw_custom_tags = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = text[pos..].find("<customWorkbookView") {
            let start = pos + rel;
            let next = text.as_bytes().get(start + "<customWorkbookView".len()).copied();
            if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                pos = start + "<customWorkbookView".len();
                continue;
            }
            let end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <customWorkbookView> tag".into()))?;
            let end = start + end_rel + 1;
            raw_custom_tags.push((start, end));
            pos = end;
        }

        if raw_custom_tags.len() != parsed_custom_views.len() {
            return Err(IoError::Format("safe sheet move could not align custom workbook views".into()));
        }

        for ((start, end), parsed) in raw_custom_tags.into_iter().zip(parsed_custom_views) {
            let Some(old_raw) = parsed.attr_u32("activeSheetId").map(|v| v as usize) else {
                continue;
            };
            let Some(&new_raw) = raw_index_map.get(&old_raw) else {
                continue;
            };
            if new_raw != old_raw {
                let replacement = set_tag_attr(&text[start..end], "activeSheetId", &new_raw.to_string())?;
                text_edits.push(TextEdit { start, end, replacement });
            }
        }
    }

    text_edits.sort_by_key(|e| e.start);
    for edit in text_edits.into_iter().rev() {
        text.replace_range(edit.start..edit.end, &edit.replacement);
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| IoError::Zip(e.to_string()))?;
    let mut out = ZipOut::new();
    let normalized_wb_part = normalize(&wb_part);

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| IoError::Zip(e.to_string()))?;
        let name = file.name().to_string();
        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes).map_err(|e| IoError::Zip(e.to_string()))?;

        if normalize(&name) == normalized_wb_part {
            out.add(&name, text.as_bytes())?;
        } else {
            out.add(&name, &bytes)?;
        }
    }

    out.finish()
}

fn patch_workbook_sheet_renames(original: &[u8], base: &Workbook, wb: &Workbook, edits: &[crate::StructuralEdit]) -> Result<Vec<u8>, IoError> {
    let mut rename_indices = BTreeSet::new();
    for edit in edits {
        if let crate::StructuralEdit::RenameSheet { sheet } = *edit {
            rename_indices.insert(sheet);
        }
    }
    if rename_indices.is_empty() {
        return Ok(original.to_vec());
    }

    let mut renames = Vec::new();
    for &sheet in &rename_indices {
        let before = base.sheet(sheet).ok_or_else(|| IoError::Format(format!("rename refers to missing base sheet {sheet}")))?;
        let after = wb.sheet(sheet).ok_or_else(|| IoError::Format(format!("rename refers to missing final sheet {sheet}")))?;
        if before.name != after.name {
            renames.push((before.name.clone(), after.name.clone()));
        }
    }
    if renames.is_empty() {
        return Ok(original.to_vec());
    }

    let mut pkg = Package::open(original)?;
    let root_rels = pkg.rels("")?;
    let wb_part = root_rels.iter().find(|r| r.kind == "officeDocument").map(|r| r.target.clone()).unwrap_or_else(|| "xl/workbook.xml".into());
    let wb_rels = pkg.rels(&wb_part)?;
    let wb_bytes = pkg.read(&wb_part)?.ok_or_else(|| IoError::Format("missing workbook part".into()))?;
    let wb_tree = xml::parse(&wb_bytes)?;
    let mut text = String::from_utf8(wb_bytes).map_err(|_| IoError::Xml(format!("{wb_part} is not UTF-8 XML")))?;

    struct TextEdit {
        start: usize,
        end: usize,
        replacement: String,
    }
    let mut text_edits = Vec::new();

    let parsed_sheets: Vec<_> = wb_tree.child("sheets").map(|s| s.kids("sheet").collect()).unwrap_or_default();
    let mut raw_sheet_spans = Vec::new();
    let mut pos = 0usize;
    while let Some(rel) = text[pos..].find("<sheet") {
        let start = pos + rel;
        let next = text.as_bytes().get(start + "<sheet".len()).copied();
        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            pos = start + "<sheet".len();
            continue;
        }
        let end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <sheet> tag in workbook.xml".into()))?;
        let end = start + end_rel + 1;
        raw_sheet_spans.push((start, end));
        pos = end;
    }

    if raw_sheet_spans.len() != parsed_sheets.len() {
        return Err(IoError::Format("safe sheet rename could not align workbook sheet tags".into()));
    }

    let mut model_sheet = 0usize;
    for ((start, end), sheet_el) in raw_sheet_spans.into_iter().zip(parsed_sheets) {
        let is_worksheet = sheet_el.attr("id").and_then(|id| wb_rels.iter().find(|r| r.id == id)).is_some_and(|r| r.kind == "worksheet");
        if !is_worksheet {
            continue;
        }

        if rename_indices.contains(&model_sheet) {
            let after = wb.sheet(model_sheet).ok_or_else(|| IoError::Format(format!("missing renamed sheet {model_sheet}")))?;
            let replacement = set_tag_attr(&text[start..end], "name", &after.name)?;
            text_edits.push(TextEdit { start, end, replacement });
        }
        model_sheet += 1;
    }

    if model_sheet != base.sheets.len() {
        return Err(IoError::Format("safe sheet rename could not align workbook worksheets with the model".into()));
    }

    let parsed_names: Vec<_> = wb_tree.child("definedNames").map(|d| d.kids("definedName").collect()).unwrap_or_default();
    if !parsed_names.is_empty() {
        let mut raw_name_bodies = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = text[pos..].find("<definedName") {
            let start = pos + rel;
            let next = text.as_bytes().get(start + "<definedName".len()).copied();
            if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                pos = start + "<definedName".len();
                continue;
            }

            let tag_end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("unterminated <definedName> tag".into()))?;
            let tag_end = start + tag_end_rel;
            let start_tag = &text[start..=tag_end];
            if start_tag.trim_end().ends_with("/>") {
                raw_name_bodies.push((tag_end + 1, tag_end + 1));
                pos = tag_end + 1;
                continue;
            }

            let body_start = tag_end + 1;
            let close_rel = text[body_start..].find("</definedName>").ok_or_else(|| IoError::Xml("definedName element has no closing tag".into()))?;
            let body_end = body_start + close_rel;
            raw_name_bodies.push((body_start, body_end));
            pos = body_end + "</definedName>".len();
        }

        if raw_name_bodies.len() != parsed_names.len() {
            return Err(IoError::Format("safe sheet rename could not align workbook defined names".into()));
        }

        for ((start, end), parsed) in raw_name_bodies.into_iter().zip(parsed_names) {
            let model_formula = crate::fmla::from_file(parsed.text.trim());
            let renamed = rename_formula_for_sheets(&model_formula, &renames);
            if renamed == model_formula {
                continue;
            }

            let raw = &text[start..end];
            let leading_len = raw.len() - raw.trim_start().len();
            let trailing_len = raw.len() - raw.trim_end().len();
            let leading = &raw[..leading_len];
            let trailing = if trailing_len == 0 { "" } else { &raw[raw.len() - trailing_len..] };
            let file_formula = crate::fmla::text_to_file(&renamed);
            text_edits.push(TextEdit { start, end, replacement: format!("{leading}{}{trailing}", xml::esc(&file_formula)) });
        }
    }

    text_edits.sort_by_key(|e| e.start);
    for edit in text_edits.into_iter().rev() {
        text.replace_range(edit.start..edit.end, &edit.replacement);
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| IoError::Zip(e.to_string()))?;
    let mut out = ZipOut::new();
    let normalized_wb_part = normalize(&wb_part);

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| IoError::Zip(e.to_string()))?;
        let name = file.name().to_string();
        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes).map_err(|e| IoError::Zip(e.to_string()))?;

        if normalize(&name) == normalized_wb_part {
            out.add(&name, text.as_bytes())?;
        } else {
            out.add(&name, &bytes)?;
        }
    }

    out.finish()
}

pub fn patch_xlsx_structural(
    original: &[u8],
    base: &Workbook,
    wb: &Workbook,
    dirty_cells: &[(usize, CellRef)],
    edits: &[crate::StructuralEdit],
) -> Result<Vec<u8>, IoError> {
    if edits.is_empty() {
        return patch_xlsx(original, base, wb, dirty_cells);
    }

    if base.sheets.len() != wb.sheets.len() {
        return Err(IoError::Format("safe structural XLSX patching requires the worksheet set to remain unchanged".into()));
    }

    let has_move = edits.iter().any(|edit| matches!(edit, crate::StructuralEdit::MoveSheet { .. }));
    if has_move {
        if edits.iter().any(|edit| !matches!(edit, crate::StructuralEdit::MoveSheet { .. })) {
            return Err(IoError::Format("safe sheet move does not yet support combining sheet moves with other structural edits".into()));
        }

        let mut order: Vec<usize> = (0..base.sheets.len()).collect();
        let mut expected_names = base.names.clone();

        for &edit in edits {
            let crate::StructuralEdit::MoveSheet { from, to } = edit else {
                continue;
            };

            if from >= order.len() {
                return Err(IoError::Format(format!("sheet move refers to missing source sheet {from}")));
            }

            let item = order.remove(from);
            let at = to.min(order.len());
            order.insert(at, item);

            for name in &mut expected_names {
                if let Some(scope) = name.scope.as_mut() {
                    *scope = match *scope {
                        x if x == from => at,
                        x if from < x && x <= at => x - 1,
                        x if at <= x && x < from => x + 1,
                        x => x,
                    };
                }
            }
        }

        for (final_index, &original_index) in order.iter().enumerate() {
            let before = base.sheet(original_index).ok_or_else(|| IoError::Format(format!("missing base sheet {original_index}")))?;
            let after = wb.sheet(final_index).ok_or_else(|| IoError::Format(format!("missing final sheet {final_index}")))?;
            if before.name != after.name {
                return Err(IoError::Format("safe sheet move found a worksheet rename or unexpected sheet order".into()));
            }
        }

        if expected_names != wb.names {
            return Err(IoError::Format("safe sheet move found defined-name changes other than scope remapping".into()));
        }

        let structurally_shifted = patch_workbook_sheet_moves(original, base, wb, edits)?;

        let mut shifted_base = base.clone();
        for &edit in edits {
            apply_structural_edit_to_baseline(&mut shifted_base, edit)?;
        }

        // Derive dirty cells from the reordered baseline instead of trusting
        // pre-move sheet indices recorded before the structural edit.
        let mut dirty: BTreeMap<usize, BTreeSet<CellRef>> = BTreeMap::new();
        for sheet_index in 0..shifted_base.sheets.len() {
            let before = shifted_base.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("missing shifted base sheet {sheet_index}")))?;
            let after = wb.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("missing final sheet {sheet_index}")))?;
            for cell_ref in before.cells.diff(&after.cells) {
                dirty.entry(sheet_index).or_default().insert(cell_ref);
            }
        }

        let dirty: Vec<(usize, CellRef)> =
            dirty.into_iter().flat_map(|(sheet, refs)| refs.into_iter().map(move |cell_ref| (sheet, cell_ref))).collect();

        return if dirty.is_empty() { Ok(structurally_shifted) } else { patch_xlsx(&structurally_shifted, &shifted_base, wb, &dirty) };
    }

    let rename_indices: BTreeSet<usize> = edits
        .iter()
        .filter_map(|edit| match *edit {
            crate::StructuralEdit::RenameSheet { sheet } => Some(sheet),
            _ => None,
        })
        .collect();

    if base.sheets.iter().zip(&wb.sheets).enumerate().any(|(i, (a, b))| a.name != b.name && !rename_indices.contains(&i)) {
        return Err(IoError::Format("safe structural XLSX patching found an unrecorded worksheet rename".into()));
    }

    if !rename_indices.is_empty() && base.sheets.iter().zip(&wb.sheets).any(|(a, b)| a.charts != b.charts) {
        return Err(IoError::Format("safe sheet rename does not yet support chart references that change with the sheet name".into()));
    }

    let renames: Vec<(String, String)> = rename_indices
        .iter()
        .filter_map(|&i| {
            let a = base.sheet(i)?;
            let b = wb.sheet(i)?;
            (a.name != b.name).then(|| (a.name.clone(), b.name.clone()))
        })
        .collect();

    let mut expected_names = base.names.clone();
    for name in &mut expected_names {
        name.formula = rename_formula_for_sheets(&name.formula, &renames);
    }
    if expected_names != wb.names {
        return Err(IoError::Format("safe structural XLSX patching does not yet support defined-name changes other than sheet renames".into()));
    }

    let structurally_shifted = structurally_shift_package(original, base, edits)?;
    let structurally_shifted = patch_workbook_sheet_renames(&structurally_shifted, base, wb, edits)?;

    let mut shifted_base = base.clone();
    for &edit in edits {
        apply_structural_edit_to_baseline(&mut shifted_base, edit)?;
    }

    let mut dirty: BTreeMap<usize, BTreeSet<CellRef>> = BTreeMap::new();

    for &(sheet_index, cell_ref) in dirty_cells {
        dirty.entry(sheet_index).or_default().insert(cell_ref);
    }

    for sheet_index in 0..shifted_base.sheets.len() {
        let before = shifted_base.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("missing shifted base sheet {sheet_index}")))?;
        let after = wb.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("missing final sheet {sheet_index}")))?;

        for cell_ref in before.cells.diff(&after.cells) {
            dirty.entry(sheet_index).or_default().insert(cell_ref);
        }
    }

    let dirty: Vec<(usize, CellRef)> = dirty.into_iter().flat_map(|(sheet, refs)| refs.into_iter().map(move |cell_ref| (sheet, cell_ref))).collect();

    if dirty.is_empty() { Ok(structurally_shifted) } else { patch_xlsx(&structurally_shifted, &shifted_base, wb, &dirty) }
}

/// Convert an edited shared-formula group into standalone formulas before
/// replacing one of its cells. Otherwise deleting the master leaves dangling
/// shared-formula children. Preserve each original cached value and cell style.
fn expand_edited_shared_formula_groups(
    source: &str,
    base: &gridcraft_model::Sheet,
    after: &gridcraft_model::Sheet,
    dirty: &BTreeSet<CellRef>,
) -> Result<String, IoError> {
    let mut group_ids = BTreeSet::<String>::new();

    for &address in dirty {
        let before = base.cell(address).and_then(|c| c.formula.as_ref());
        let after = after.cell(address).and_then(|c| c.formula.as_ref());
        if before == after {
            continue;
        }
        let Some((start, end, _)) = find_cell_span(source, &address.a1()) else {
            continue;
        };
        let cell = &source[start..end];
        let Some(f_start) = cell.find("<f") else { continue };
        let f_end = f_start + cell[f_start..]
            .find('>')
            .ok_or_else(|| IoError::Xml("shared formula start tag is unterminated".into()))?;
        let f_tag = &cell[f_start..=f_end];
        if attr_value(f_tag, "t").as_deref() == Some("shared") {
            let si = attr_value(f_tag, "si")
                .ok_or_else(|| IoError::Format("shared formula group has no si attribute".into()))?;
            group_ids.insert(si);
        }
    }

    if group_ids.is_empty() {
        return Ok(source.to_string());
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = source[cursor..].find("<c") {
        let cell_start = cursor + rel;
        let next = source.as_bytes().get(cell_start + 2).copied();
        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            cursor = cell_start + 2;
            continue;
        }
        let tag_end = cell_start + source[cell_start..]
            .find('>')
            .ok_or_else(|| IoError::Xml("worksheet cell tag is unterminated".into()))?;
        let cell_tag = &source[cell_start..=tag_end];
        if cell_tag.trim_end().ends_with("/>") {
            cursor = tag_end + 1;
            continue;
        }
        let cell_end = tag_end + 1 + source[tag_end + 1..]
            .find("</c>")
            .ok_or_else(|| IoError::Xml("worksheet cell has no closing tag".into()))? + 4;
        cursor = cell_end;
        let cell_xml = &source[cell_start..cell_end];

        let Some(relative_f_start) = cell_xml.find("<f") else { continue };
        let f_start = cell_start + relative_f_start;
        let f_tag_end = f_start + source[f_start..]
            .find('>')
            .ok_or_else(|| IoError::Xml("formula tag is unterminated".into()))?;
        let f_tag = &source[f_start..=f_tag_end];
        if attr_value(f_tag, "t").as_deref() != Some("shared") {
            continue;
        }
        let si = attr_value(f_tag, "si")
            .ok_or_else(|| IoError::Format("shared formula group has no si attribute".into()))?;
        if !group_ids.contains(&si) {
            continue;
        }

        let address = attr_value(cell_tag, "r")
            .and_then(|s| CellRef::parse(&s))
            .ok_or_else(|| IoError::Format("shared formula has no valid cell address".into()))?;
        let formula = base.cell(address).and_then(|c| c.formula.as_ref())
            .ok_or_else(|| IoError::Format(format!("shared formula missing in model at {}!{}", base.name, address.a1())))?;

        let f_end = if f_tag.trim_end().ends_with("/>") {
            f_tag_end + 1
        } else {
            f_tag_end + 1 + source[f_tag_end + 1..cell_end]
                .find("</f>")
                .ok_or_else(|| IoError::Xml("shared formula has no closing tag".into()))? + 4
        };
        replacements.push((f_start, f_end, format!("<f>{}</f>", xml::esc(&crate::fmla::to_file(formula)))));
    }

    let mut result = source.to_string();
    for (start, end, replacement) in replacements.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Ok(result)
}

/// Preserve explicit row heights, hidden rows and outline metadata changed by
/// auto-fit, copy or row formatting. Cell edits otherwise create plain <row r>
/// elements and silently discard the model's new row height.
fn patch_row_metadata(
    source: &str,
    before: &gridcraft_model::Sheet,
    after: &gridcraft_model::Sheet,
) -> Result<String, IoError> {
    if before.rows == after.rows {
        return Ok(source.to_string());
    }
    let changed: BTreeSet<u32> = before.rows.keys().chain(after.rows.keys()).copied()
        .filter(|r| before.rows.get(r) != after.rows.get(r)).collect();
    let mut result = source.to_string();
    for row in changed {
        let old_info = before.rows.get(&row);
        let new_info = after.rows.get(&row);
        if old_info.and_then(|r| r.style) != new_info.and_then(|r| r.style) {
            return Err(IoError::Format(format!(
                "safe worksheet row patch does not yet support changing the row's base style at row {}", row + 1
            )));
        }
        let row_number = row.saturating_add(1);
        let data_start = result.find("<sheetData")
            .ok_or_else(|| IoError::Xml("worksheet has no sheetData element".into()))?;
        let data_tag_end = data_start + result[data_start..].find('>')
            .ok_or_else(|| IoError::Xml("unterminated sheetData tag".into()))? + 1;
        let self_closing_data = result[data_start..data_tag_end].trim_end().ends_with("/>");
        let data_end = if self_closing_data {
            data_tag_end
        } else {
            data_tag_end + result[data_tag_end..].find("</sheetData>")
                .ok_or_else(|| IoError::Xml("sheetData has no closing tag".into()))?
        };

        let mut position = data_tag_end;
        let mut existing = None;
        if !self_closing_data {
            while position < data_end {
                let Some(relative) = result[position..data_end].find("<row") else {
                    position = data_end;
                    break;
                };
                let row_start = position + relative;
                let next = result.as_bytes().get(row_start + 4).copied();
                if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                    position = row_start + 4;
                    continue;
                }
                let row_end = row_start + result[row_start..].find('>')
                    .ok_or_else(|| IoError::Xml("unterminated row tag".into()))? + 1;
                let xml_row = attr_value(&result[row_start..row_end], "r")
                    .and_then(|s| s.parse::<u32>().ok())
                    .ok_or_else(|| IoError::Format("worksheet row has no valid number".into()))?;
                if xml_row == row_number {
                    existing = Some((row_start, row_end));
                    position = row_start;
                    break;
                }
                if xml_row > row_number {
                    position = row_start;
                    break;
                }
                position = row_end;
            }
        }

        let mut tag = if let Some((start, end)) = existing {
            result[start..end].to_string()
        } else {
            format!("<row r=\"{row_number}\"/>")
        };
        for attr in ["ht", "customHeight", "hidden", "outlineLevel", "collapsed"] {
            tag = remove_attr(&tag, attr);
        }
        if let Some(info) = new_info {
            if let Some(height) = info.size {
                tag = set_tag_attr(&tag, "ht", &height.to_string())?;
                tag = set_tag_attr(&tag, "customHeight", "1")?;
            }
            if info.hidden {
                tag = set_tag_attr(&tag, "hidden", "1")?;
            }
            if info.outline > 0 {
                tag = set_tag_attr(&tag, "outlineLevel", &info.outline.to_string())?;
            }
            if info.collapsed {
                tag = set_tag_attr(&tag, "collapsed", "1")?;
            }
        }
        if let Some((start, end)) = existing {
            result.replace_range(start..end, &tag);
        } else if new_info.is_some() {
            if self_closing_data {
                let expanded = format!("<sheetData>{tag}</sheetData>");
                result.replace_range(data_start..data_tag_end, &expanded);
            } else {
                result.insert_str(position, &tag);
            }
        }
    }
    Ok(result)
}

/// Patch the sheet's merge list without rebuilding any unrelated OOXML parts.
/// Excel stores these ranges outside sheetData, so a cell-only patch used to
/// silently discard copied or newly merged areas.
fn patch_merged_ranges(
    source: &str,
    before: &[gridcraft_core::RangeRef],
    after: &[gridcraft_core::RangeRef],
) -> Result<String, IoError> {
    if before == after {
        return Ok(source.to_string());
    }
    let mut unique = BTreeSet::new();
    for range in after {
        if !unique.insert(range.a1()) {
            return Err(IoError::Format(format!("duplicate merged range {}", range.a1())));
        }
    }
    for (i, a) in after.iter().enumerate() {
        for b in after.iter().skip(i + 1) {
            if a.intersects(b) {
                return Err(IoError::Format(format!("overlapping merged ranges {} and {}", a.a1(), b.a1())));
            }
        }
    }

    let before_refs: BTreeSet<String> = before.iter().map(|r| r.a1()).collect();
    let mut original_refs = BTreeSet::new();
    let mut location = None;
    if let Some(start) = source.find("<mergeCells") {
        let start_end = start + source[start..]
            .find('>')
            .ok_or_else(|| IoError::Xml("unterminated mergeCells start tag".into()))? + 1;
        let end = if source[start..start_end].trim_end().ends_with("/>") {
            start_end
        } else {
            start_end + source[start_end..]
                .find("</mergeCells>")
                .ok_or_else(|| IoError::Xml("unterminated mergeCells element".into()))? + "</mergeCells>".len()
        };

        let merge_segment = &source[start..end];
        let mut cursor = 0usize;
        while let Some(relative) = merge_segment[cursor..].find("<mergeCell") {
            let tag_start = cursor + relative;
            let next = merge_segment.as_bytes().get(tag_start + "<mergeCell".len()).copied();
            if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'/') | Some(b'>')) {
                cursor = tag_start + "<mergeCell".len();
                continue;
            }
            let tag_end = tag_start + merge_segment[tag_start..]
                .find('>')
                .ok_or_else(|| IoError::Xml("unterminated mergeCell tag".into()))? + 1;
            let tag = &merge_segment[tag_start..tag_end];
            let reference = attr_value(tag, "ref")
                .ok_or_else(|| IoError::Format("mergeCell has no ref".into()))?;
            let range = gridcraft_core::RangeRef::parse(&reference)
                .ok_or_else(|| IoError::Format(format!("invalid mergeCell ref {reference}")))?;
            original_refs.insert(range.a1());
            cursor = tag_end;
        }
        location = Some((start, end));
    }
    if original_refs != before_refs {
        return Err(IoError::Format(
            "original merged ranges do not match the imported workbook; refusing lossy merge rewrite".into(),
        ));
    }

    let mut replacement = format!("<mergeCells count=\"{}\">", after.len());
    for range in after {
        replacement.push_str(&format!("<mergeCell ref=\"{}\"/>", xml::esc_attr(&range.a1())));
    }
    replacement.push_str("</mergeCells>");

    let mut result = source.to_string();
    if let Some((start, end)) = location {
        result.replace_range(start..end, &replacement);
    } else {
        // OOXML places mergeCells after sortState/customSheetViews, and
        // before conditionalFormatting/dataValidations/pageMargins/drawings.
        // Insert at the first following element with known later schema order.
        let after_data = source.find("</sheetData>").map(|p| p + "</sheetData>".len())
            .or_else(|| source.find("<sheetData").and_then(|p| source[p..].find('>').map(|end| p + end + 1)))
            .ok_or_else(|| IoError::Xml("worksheet has no sheetData element".into()))?;
        let after_tags = [
            "<conditionalFormatting", "<dataValidations", "<hyperlinks",
            "<printOptions", "<pageMargins", "<pageSetup", "<headerFooter",
            "<rowBreaks", "<colBreaks", "<customProperties", "<cellWatches",
            "<ignoredErrors", "<smartTags", "<drawing", "<legacyDrawing",
            "<picture", "<oleObjects", "<controls", "<webPublishItems",
            "<tableParts", "<extLst", "</worksheet>",
        ];
        let insert_at = after_tags.iter().filter_map(|tag| {
            source[after_data..].find(tag).map(|p| after_data + p)
        }).min().ok_or_else(|| IoError::Xml("worksheet is missing a closing element".into()))?;
        result.insert_str(insert_at, &replacement);
    }
    Ok(result)
}

pub fn patch_xlsx(original: &[u8], base: &Workbook, wb: &Workbook, dirty_cells: &[(usize, CellRef)]) -> Result<Vec<u8>, IoError> {
    patch_xlsx_with_style_copies(original, base, wb, dirty_cells, &[])
}

/// Same as patch_xlsx, plus original-source OOXML style hints for copy/fill.
/// Multiple original XF records may normalize to the same model StyleId. A
/// copied cell must keep its *exact* source XF, not an arbitrary equivalent.
pub fn patch_xlsx_with_style_copies(
    original: &[u8],
    base: &Workbook,
    wb: &Workbook,
    dirty_cells: &[(usize, CellRef)],
    style_copies: &[(usize, CellRef, usize, CellRef)],
) -> Result<Vec<u8>, IoError> {
    if dirty_cells.is_empty()
        && base.sheets.iter().zip(&wb.sheets).all(|(a, b)| a.merges == b.merges && a.rows == b.rows)
    {
        return Ok(original.to_vec());
    }

    let mut pkg = Package::open(original)?;

    let root_rels = pkg.rels("")?;
    let wb_part = root_rels.iter().find(|r| r.kind == "officeDocument").map(|r| r.target.clone()).unwrap_or_else(|| "xl/workbook.xml".into());

    let wb_xml = pkg.read_xml(&wb_part)?.ok_or_else(|| IoError::Format("no workbook part".into()))?;

    let wb_rels = pkg.rels(&wb_part)?;

    let mut sheet_parts = Vec::new();
    if let Some(sheets) = wb_xml.child("sheets") {
        for sheet in sheets.kids("sheet") {
            let Some(id) = sheet.attr("id") else {
                continue;
            };
            let Some(rel) = wb_rels.iter().find(|r| r.id == id) else {
                continue;
            };
            if rel.kind == "worksheet" {
                sheet_parts.push(rel.target.clone());
            }
        }
    }

    // Read the original s="..." XF indices once per source worksheet. This
    // avoids applying a normalized but visually different style on paste.
    let mut wanted: BTreeMap<usize, BTreeSet<CellRef>> = BTreeMap::new();
    for &(src_sheet, src_cell, _, _) in style_copies {
        wanted.entry(src_sheet).or_default().insert(src_cell);
    }
    let mut source_xfs: HashMap<(usize, CellRef), u32> = HashMap::new();
    for (src_sheet, refs) in wanted {
        let Some(part) = sheet_parts.get(src_sheet) else { continue };
        let Some(bytes) = pkg.read(part)? else { continue };
        let xml_text = std::str::from_utf8(&bytes)
            .map_err(|_| IoError::Xml(format!("{part} is not UTF-8 XML")))?;
        let bytes = xml_text.as_bytes();
        let mut pos = 0usize;
        while let Some(relative) = xml_text[pos..].find("<c") {
            let start = pos + relative;
            let next = bytes.get(start + 2).copied();
            if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                pos = start + 2;
                continue;
            }
            let end = start + xml_text[start..].find('>')
                .ok_or_else(|| IoError::Xml("worksheet cell start tag is unterminated".into()))? + 1;
            let tag = &xml_text[start..end];
            if let Some(cell) = attr_value(tag, "r").and_then(|r| CellRef::parse(&r))
                && refs.contains(&cell)
            {
                let xf = attr_value(tag, "s").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
                source_xfs.insert((src_sheet, cell), xf);
                if source_xfs.len() >= style_copies.len() {
                    break;
                }
            }
            pos = end;
        }
    }
    let mut exact_styles: HashMap<(usize, CellRef), u32> = HashMap::new();
    for &(src_sheet, src_cell, dst_sheet, dst_cell) in style_copies {
        let source_style = base.sheet(src_sheet).and_then(|s| s.cell(src_cell)).map(|c| c.style);
        let target_style = wb.sheet(dst_sheet).and_then(|s| s.cell(dst_cell)).map(|c| c.style);
        if source_style.is_some() && source_style == target_style {
            if let Some(&xf) = source_xfs.get(&(src_sheet, src_cell)) {
                exact_styles.insert((dst_sheet, dst_cell), xf);
            }
        }
    }

    let sst_part = wb_rels.iter().find(|r| r.kind == "sharedStrings").map(|r| r.target.clone());

    let mut sst = PatchSst::load(&mut pkg, sst_part)?;

    let styles_part = wb_rels.iter().find(|r| r.kind == "styles").map(|r| r.target.clone());
    let mut styles = PatchStyles::load(&mut pkg, styles_part)?;

    let mut dirty_by_sheet: BTreeMap<usize, BTreeSet<CellRef>> = BTreeMap::new();
    for &(sheet_index, cell_ref) in dirty_cells {
        dirty_by_sheet.entry(sheet_index).or_default().insert(cell_ref);
    }
    for sheet_index in 0..base.sheets.len().min(wb.sheets.len()) {
        if base.sheet(sheet_index).map(|s| (&s.merges, &s.rows))
            != wb.sheet(sheet_index).map(|s| (&s.merges, &s.rows))
        {
            dirty_by_sheet.entry(sheet_index).or_default();
        }
    }

    let mut replacements: HashMap<String, Vec<u8>> = HashMap::new();

    for (sheet_index, refs) in dirty_by_sheet {
        let Some(base_sheet) = base.sheet(sheet_index) else {
            return Err(IoError::Format(format!("dirty cell refers to missing base sheet {sheet_index}")));
        };
        let Some(sheet) = wb.sheet(sheet_index) else {
            return Err(IoError::Format(format!("dirty cell refers to missing sheet {sheet_index}")));
        };
        let Some(part) = sheet_parts.get(sheet_index) else {
            return Err(IoError::Format(format!("cannot resolve worksheet part for sheet {sheet_index}")));
        };

        let bytes = pkg.read(part)?.ok_or_else(|| IoError::Format(format!("missing worksheet part {part}")))?;

        let text = String::from_utf8(bytes).map_err(|_| IoError::Xml(format!("{part} is not UTF-8 XML")))?;
        let mut text = expand_edited_shared_formula_groups(&text, base_sheet, sheet, &refs)?;

        struct Edit {
            start: usize,
            end: usize,
            replacement: String,
        }

        let mut edits = Vec::new();
        let mut new_cells = Vec::new();
        let removed_rows: BTreeSet<u32> = refs.iter()
            .filter(|cell| sheet.cell(**cell).is_none())
            .map(|cell| cell.row)
            .collect();

        for cell_ref in refs {
            let old_style = base_sheet.style_id(cell_ref);
            let new_style = sheet.style_id(cell_ref);

            let style_index = if let Some(&exact_xf) = exact_styles.get(&(sheet_index, cell_ref)) {
                Some(exact_xf)
            } else if old_style != new_style {
                if new_style.0 == 0 {
                    Some(0)
                } else {
                    Some(styles.style_index(wb.styles.get(new_style)).map_err(|error| {
                        IoError::Format(format!("safe XLSX patching could not apply style at {}!{}: {error}", sheet.name, cell_ref.a1()))
                    })?)
                }
            } else {
                None
            };

            let old_cell = base_sheet.cell(cell_ref);
            let new_cell = sheet.cell(cell_ref);

            if old_cell == new_cell && style_index.is_none() {
                continue;
            }

            let old_formula = old_cell.and_then(|c| c.formula.as_ref());
            let new_formula = new_cell.and_then(|c| c.formula.as_ref());

            // A dependent formula may have received a new cached result after recalculation.
            // Preserve an unchanged formula cell exactly; Excel can recalculate it.
            if old_formula.is_some() && old_formula == new_formula && style_index.is_none() {
                continue;
            }

            let a1 = cell_ref.a1();

            let Some((start, end, start_tag_end)) = find_cell_span(&text, &a1) else {
                // A cell absent in both the destination model and original XML
                // must stay absent. Creating a synthetic blank here changes the
                // worksheet's used range after an undo.
                if new_cell.is_none() {
                    continue;
                }
                if old_formula.is_some() {
                    return Err(IoError::Format(format!(
                        "safe XLSX patching found a modeled formula with no original cell at {}!{}",
                        sheet.name, a1
                    )));
                }

                let synthetic = format!(r#"<c r="{a1}"/>"#);

                let (mut replacement, new_shared) = if let Some(formula) = new_formula {
                    let raw_value = new_cell.map(|cell| &cell.value).unwrap_or(&Value::Empty);
                    (write_formula_cell(&synthetic, formula, raw_value)?, false)
                } else {
                    let value = new_cell.map(|cell| cell.value.scalar()).unwrap_or(Value::Empty);
                    write_constant_cell(&synthetic, &value, &mut sst)?
                };

                let new_cell_style_index = if let Some(style_index) = style_index {
                    Some(style_index)
                } else if new_style.0 != 0 {
                    Some(styles.style_index(wb.styles.get(new_style)).map_err(|error| {
                        IoError::Format(format!("safe XLSX patching could not apply style at {}!{}: {error}", sheet.name, cell_ref.a1()))
                    })?)
                } else {
                    None
                };

                if let Some(style_index) = new_cell_style_index {
                    replacement = set_cell_style(&replacement, style_index)?;
                }

                if new_shared {
                    sst.count_delta += 1;
                }

                new_cells.push((cell_ref, replacement));
                continue;
            };

            let original_cell = &text[start..end];

            if new_cell.is_none() {
                // Removing a copied/filled cell must remove its OOXML node,
                // not leave behind <c r="..." s="..."/>. This matters when
                // Undo follows a successful Save on a macro-enabled workbook.
                // Refuse to erase raw XML that wasn't represented in the model.
                if old_cell.is_none() {
                    return Err(IoError::Format(format!(
                        "cannot safely remove an unmodeled Excel cell at {}!{}", sheet.name, a1
                    )));
                }
                if attr_value(&text[start..start_tag_end], "t").as_deref() == Some("s") {
                    sst.count_delta -= 1;
                }
                edits.push(Edit { start, end, replacement: String::new() });
                continue;
            }

            if let Some(style_index) = style_index
                && old_formula == new_formula
                && (old_formula.is_some() || old_cell.map(|cell| &cell.value) == new_cell.map(|cell| &cell.value))
            {
                edits.push(Edit { start, end, replacement: set_cell_style(original_cell, style_index)? });
                continue;
            }

            let original_start = &text[start..start_tag_end];
            let old_type = attr_value(original_start, "t");
            let old_shared = old_type.as_deref() == Some("s");

            let value = new_cell.map(|cell| cell.value.scalar()).unwrap_or(Value::Empty);

            let (mut replacement, new_shared) = match (old_formula, new_formula) {
                (Some(_), Some(formula)) => {
                    let raw_value = new_cell.map(|cell| &cell.value).unwrap_or(&Value::Empty);
                    (write_formula_cell(original_cell, formula, raw_value)?, false)
                }
                (Some(_), None) => write_constant_cell(original_cell, &value, &mut sst)?,
                (None, Some(formula)) => {
                    let raw_value = new_cell.map(|cell| &cell.value).unwrap_or(&Value::Empty);
                    (write_formula_cell(original_cell, formula, raw_value)?, false)
                }
                (None, None) => write_constant_cell(original_cell, &value, &mut sst)?,
            };

            if let Some(style_index) = style_index {
                replacement = set_cell_style(&replacement, style_index)?;
            }

            match (old_shared, new_shared) {
                (true, false) => sst.count_delta -= 1,
                (false, true) => sst.count_delta += 1,
                _ => {}
            }

            edits.push(Edit { start, end, replacement });
        }

        edits.sort_by_key(|a| std::cmp::Reverse(a.start));
        for edit in edits {
            text.replace_range(edit.start..edit.end, &edit.replacement);
        }

        for (cell_ref, cell_xml) in new_cells {
            let (start, end, replacement) = new_cell_edit(&text, cell_ref, &cell_xml)?;
            text.replace_range(start..end, &replacement);
        }

        let mut text = patch_row_metadata(&text, base_sheet, sheet)?;
        // A newly created row may be empty after Undo. Keep original rows
        // that have actual content or row properties, but remove synthetic
        // wrappers that would otherwise expand Excel's used range.
        for row in removed_rows {
            if sheet.rows.contains_key(&row)
                || sheet.cells.row(row, 0, gridcraft_core::MAX_COLS - 1).next().is_some()
            {
                continue;
            }
            let number = row + 1;
            text = text.replace(&format!(r#"<row r="{number}"></row>"#), "");
            text = text.replace(&format!(r#"<row r="{number}"/>"#), "");
        }
        let text = patch_merged_ranges(&text, &base_sheet.merges, &sheet.merges)?;
        replacements.insert(normalize(part), text.into_bytes());
    }

    if let Some((part, bytes)) = styles.replacement() {
        replacements.insert(normalize(part), bytes);
    }

    if sst.changed() {
        let Some(part) = &sst.part else {
            return Err(IoError::Format("shared string changes require an original sharedStrings part".into()));
        };

        let bytes = pkg.read(part)?.ok_or_else(|| IoError::Format(format!("missing shared strings part {part}")))?;

        let patched = patch_shared_strings(&bytes, &sst)?;
        replacements.insert(normalize(part), patched);
    }

    let mut archive = zip::ZipArchive::new(Cursor::new(original)).map_err(|e| IoError::Zip(e.to_string()))?;
    let mut out = ZipOut::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| IoError::Zip(e.to_string()))?;
        let name = file.name().to_string();

        let mut bytes = Vec::with_capacity(file.size() as usize);
        file.read_to_end(&mut bytes).map_err(|e| IoError::Zip(e.to_string()))?;

        if let Some(replacement) = replacements.get(&normalize(&name)) {
            out.add(&name, replacement)?;
        } else {
            out.add(&name, &bytes)?;
        }
    }

    out.finish()
}

fn set_cell_style(cell: &str, style_index: u32) -> Result<String, IoError> {
    let tag_end = cell.find('>').ok_or_else(|| IoError::Xml("cell start tag is not closed".into()))?;
    let start = &cell[..=tag_end];
    let self_closing = start[..start.len() - 1].trim_end().ends_with('/');

    let mut open = start[..start.len() - 1].trim_end().to_string();
    if open.ends_with('/') {
        open.pop();
        open = open.trim_end().to_string();
    }

    open = remove_attr(&open, "s");

    if style_index != 0 {
        open.push_str(&format!(r#" s="{style_index}""#));
    }

    let mut out = String::with_capacity(cell.len() + 16);
    out.push_str(&open);
    if self_closing {
        out.push_str("/>");
    } else {
        out.push('>');
    }
    out.push_str(&cell[tag_end + 1..]);
    Ok(out)
}

fn write_formula_cell(original_cell: &str, formula: &gridcraft_model::Formula, value: &Value) -> Result<String, IoError> {
    let tag_end = original_cell.find('>').ok_or_else(|| IoError::Xml("cell start tag is not closed".into()))?;
    let original_start = &original_cell[..=tag_end];

    if attr_value(original_start, "cm").is_some() || formula.array.is_some() || matches!(value, Value::Array(_)) {
        return Err(IoError::Format("safe XLSX patching does not yet support array or dynamic formula edits".into()));
    }

    if let Some(formula_start) = original_cell.find("<f") {
        let formula_tag_end_rel =
            original_cell[formula_start..].find('>').ok_or_else(|| IoError::Xml("formula element start tag is not closed".into()))?;
        let formula_tag_end = formula_start + formula_tag_end_rel;
        let formula_start_tag = &original_cell[formula_start..=formula_tag_end];

        if formula_start_tag.trim() != "<f>" {
            return Err(IoError::Format("safe XLSX patching does not yet support shared, array, or attributed formula elements".into()));
        }
    }

    let mut open = original_start[..original_start.len() - 1].trim_end().to_string();
    if open.ends_with('/') {
        open.pop();
        open = open.trim_end().to_string();
    }
    open = remove_attr(&open, "t");

    let value = match value {
        Value::Error(CellError::Circ) => Value::Number(0.0),
        Value::Error(CellError::Spill | CellError::Calc) => Value::Error(CellError::Value),
        value => value.clone(),
    };

    let cell_type = match &value {
        Value::Text(_) => " t=\"str\"",
        Value::Bool(_) => " t=\"b\"",
        Value::Error(_) => " t=\"e\"",
        _ => "",
    };

    let formula_text = xml::esc(&crate::fmla::to_file(formula));
    let mut out = format!("{open}{cell_type}><f>{formula_text}</f>");

    match &value {
        Value::Number(number) => out.push_str(&format!("<v>{}</v>", xml::num(*number))),
        Value::Text(text) => out.push_str(&format!("<v>{}</v>", xml::esc(&xml::encode_xstring(text)))),
        Value::Bool(value) => out.push_str(&format!("<v>{}</v>", u8::from(*value))),
        Value::Error(error) => out.push_str(&format!("<v>{}</v>", error.as_str())),
        Value::Empty => {}
        Value::Array(_) => return Err(IoError::Format("array formula cache is not supported by safe XLSX patching".into())),
    }

    out.push_str("</c>");
    Ok(out)
}

fn write_constant_cell(original_cell: &str, value: &Value, sst: &mut PatchSst) -> Result<(String, bool), IoError> {
    let tag_end = original_cell.find('>').ok_or_else(|| IoError::Xml("cell start tag is not closed".into()))?;

    let original_start = &original_cell[..=tag_end];
    let old_type = attr_value(original_start, "t");

    let mut open = original_start[..original_start.len() - 1].trim_end().to_string();

    if open.ends_with('/') {
        open.pop();
        open = open.trim_end().to_string();
    }

    open = remove_attr(&open, "t");

    match value {
        Value::Empty | Value::Array(_) => Ok((format!("{open}/>"), false)),
        Value::Number(number) => Ok((format!("{open}><v>{number}</v></c>"), false)),
        Value::Bool(value) => Ok((format!("{open} t=\"b\"><v>{}</v></c>", u8::from(*value)), false)),
        Value::Error(error) => Ok((format!("{open} t=\"e\"><v>{}</v></c>", error.as_str()), false)),
        Value::Text(text) => {
            if old_type.as_deref() == Some("inlineStr") || sst.part.is_none() {
                Ok((format!("{open} t=\"inlineStr\"><is>{}</is></c>", xml::t_el(text)), false))
            } else {
                let index = sst.index(text)?;
                Ok((format!("{open} t=\"s\"><v>{index}</v></c>"), true))
            }
        }
    }
}

fn patch_shared_strings(bytes: &[u8], sst: &PatchSst) -> Result<Vec<u8>, IoError> {
    let mut text = String::from_utf8(bytes.to_vec()).map_err(|_| IoError::Xml("sharedStrings.xml is not UTF-8".into()))?;

    let start = text.find("<sst").ok_or_else(|| IoError::Xml("sharedStrings.xml has no <sst> root".into()))?;
    let start_end_rel = text[start..].find('>').ok_or_else(|| IoError::Xml("sharedStrings.xml has an unterminated <sst> root".into()))?;
    let start_end = start + start_end_rel;

    let root = text[start..start_end].to_string();
    let mut new_root = remove_attr(&root, "count");
    new_root = remove_attr(&new_root, "uniqueCount");
    text.replace_range(start..start_end, &new_root);

    if !sst.new_strings.is_empty() {
        let close = text.rfind("</sst>").ok_or_else(|| IoError::Xml("sharedStrings.xml has no </sst>".into()))?;

        let mut added = String::new();
        for value in &sst.new_strings {
            added.push_str("<si>");
            added.push_str(&xml::t_el(value));
            added.push_str("</si>");
        }
        text.insert_str(close, &added);
    }

    Ok(text.into_bytes())
}

fn find_cell_span(xml: &str, wanted: &str) -> Option<(usize, usize, usize)> {
    let bytes = xml.as_bytes();
    let mut pos = 0usize;

    while let Some(rel) = xml[pos..].find("<c") {
        let start = pos + rel;
        let after_c = start + 2;

        let next = bytes.get(after_c).copied();
        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            pos = after_c;
            continue;
        }

        let tag_end_rel = xml[start..].find('>')?;
        let tag_end = start + tag_end_rel + 1;
        let start_tag = &xml[start..tag_end];

        if attr_value(start_tag, "r").as_deref() != Some(wanted) {
            pos = tag_end;
            continue;
        }

        if start_tag.trim_end().ends_with("/>") {
            return Some((start, tag_end, tag_end));
        }

        let close_rel = xml[tag_end..].find("</c>")?;
        let end = tag_end + close_rel + 4;
        return Some((start, end, tag_end));
    }

    None
}

fn new_cell_edit(xml: &str, cell_ref: CellRef, cell_xml: &str) -> Result<(usize, usize, String), IoError> {
    let target_row = cell_ref.row.saturating_add(1);

    let sheet_data_start = xml.find("<sheetData").ok_or_else(|| IoError::Xml("worksheet has no <sheetData> element".into()))?;
    let sheet_data_tag_end_rel =
        xml[sheet_data_start..].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <sheetData> element".into()))?;
    let sheet_data_tag_end = sheet_data_start + sheet_data_tag_end_rel + 1;
    let sheet_data_start_tag = &xml[sheet_data_start..sheet_data_tag_end];

    if sheet_data_start_tag.trim_end().ends_with("/>") {
        let mut open = sheet_data_start_tag[..sheet_data_start_tag.len() - 1].trim_end().to_string();
        if open.ends_with('/') {
            open.pop();
            open = open.trim_end().to_string();
        }

        return Ok((sheet_data_start, sheet_data_tag_end, format!("{open}><row r=\"{target_row}\">{cell_xml}</row></sheetData>")));
    }

    let sheet_data_close_rel =
        xml[sheet_data_tag_end..].find("</sheetData>").ok_or_else(|| IoError::Xml("worksheet has no </sheetData> element".into()))?;
    let sheet_data_close = sheet_data_tag_end + sheet_data_close_rel;

    let bytes = xml.as_bytes();
    let mut pos = sheet_data_tag_end;

    while pos < sheet_data_close {
        let Some(rel) = xml[pos..sheet_data_close].find("<row") else {
            break;
        };

        let row_start = pos + rel;
        let after_row = row_start + 4;
        let next = bytes.get(after_row).copied();
        if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
            pos = after_row;
            continue;
        }

        let row_tag_end_rel =
            xml[row_start..sheet_data_close].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <row> element".into()))?;
        let row_tag_end = row_start + row_tag_end_rel + 1;
        let row_start_tag = &xml[row_start..row_tag_end];

        let row_number = attr_value(row_start_tag, "r").and_then(|value| value.parse::<u32>().ok());

        if row_number == Some(target_row) {
            if row_start_tag.trim_end().ends_with("/>") {
                let mut open = row_start_tag[..row_start_tag.len() - 1].trim_end().to_string();
                if open.ends_with('/') {
                    open.pop();
                    open = open.trim_end().to_string();
                }

                return Ok((row_start, row_tag_end, format!("{open}>{cell_xml}</row>")));
            }

            let row_close_rel =
                xml[row_tag_end..sheet_data_close].find("</row>").ok_or_else(|| IoError::Xml("worksheet row has no </row> element".into()))?;
            let row_close = row_tag_end + row_close_rel;

            let mut cell_pos = row_tag_end;
            while cell_pos < row_close {
                let Some(cell_rel) = xml[cell_pos..row_close].find("<c") else {
                    break;
                };

                let cell_start = cell_pos + cell_rel;
                let after_c = cell_start + 2;
                let next = bytes.get(after_c).copied();
                if !matches!(next, Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | Some(b'>') | Some(b'/')) {
                    cell_pos = after_c;
                    continue;
                }

                let cell_tag_end_rel =
                    xml[cell_start..row_close].find('>').ok_or_else(|| IoError::Xml("worksheet has an unterminated <c> element".into()))?;
                let cell_tag_end = cell_start + cell_tag_end_rel + 1;
                let cell_start_tag = &xml[cell_start..cell_tag_end];

                if let Some(existing) = attr_value(cell_start_tag, "r").and_then(|value| CellRef::parse(&value))
                    && existing.col > cell_ref.col
                {
                    return Ok((cell_start, cell_start, cell_xml.to_string()));
                }

                cell_pos = cell_tag_end;
            }

            return Ok((row_close, row_close, cell_xml.to_string()));
        }

        if row_number.is_some_and(|row| row > target_row) {
            return Ok((row_start, row_start, format!("<row r=\"{target_row}\">{cell_xml}</row>")));
        }

        if row_start_tag.trim_end().ends_with("/>") {
            pos = row_tag_end;
        } else {
            let row_close_rel =
                xml[row_tag_end..sheet_data_close].find("</row>").ok_or_else(|| IoError::Xml("worksheet row has no </row> element".into()))?;
            pos = row_tag_end + row_close_rel + 6;
        }
    }

    Ok((sheet_data_close, sheet_data_close, format!("<row r=\"{target_row}\">{cell_xml}</row>")))
}

fn attr_value(tag: &str, wanted: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>' && bytes[i] != b'/' {
            i += 1;
        }

        if i == name_start {
            i = i.saturating_add(1);
            continue;
        }

        let name = &tag[name_start..i];

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let quote = *bytes.get(i)?;
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        i += 1;

        let value_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }

        if name == wanted {
            return Some(tag[value_start..i].to_string());
        }

        i = i.saturating_add(1);
    }

    None
}

fn remove_attr(tag: &str, wanted: &str) -> String {
    let bytes = tag.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if !bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        let remove_start = i;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'>' && bytes[i] != b'/' {
            i += 1;
        }

        if &tag[name_start..i] != wanted {
            continue;
        }

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;

        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        let Some(&quote) = bytes.get(i) else {
            break;
        };
        if quote != b'"' && quote != b'\'' {
            continue;
        }

        i += 1;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if i < bytes.len() {
            i += 1;
        }

        let mut out = String::with_capacity(tag.len());
        out.push_str(&tag[..remove_start]);
        out.push_str(&tag[i..]);
        return out;
    }

    tag.to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Cursor, Read};
    use std::sync::Arc;

    use gridcraft_core::{CellRef, Value};
    use gridcraft_model::{Cell, DefinedName, Formula, NumFmt};

    use super::{patch_xlsx, patch_xlsx_structural, shift_sheet_data_coordinates};
    use crate::read_xlsx;

    fn at(a1: &str) -> CellRef {
        CellRef::parse(a1).unwrap()
    }

    fn parts(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut out = BTreeMap::new();

        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            if file.is_dir() {
                continue;
            }
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            out.insert(file.name().to_string(), data);
        }

        out
    }

    fn assert_only_sheet_changed(before: &[u8], after: &[u8]) {
        let before = parts(before);
        let after = parts(after);

        assert_eq!(before.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>());

        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/worksheets/sheet1.xml"]);
    }

    #[test]
    fn structural_rename_sheet_preserves_package_and_updates_references() {
        let wb_xml = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><bookViews><workbookView activeTab="0"/></bookViews><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Calc" sheetId="2" r:id="rId2"/></sheets><definedNames><definedName name="Owned">Data!$A$1</definedName><definedName name="_xlnm.Print_Area" localSheetId="0">Data!$A$1:$B$2</definedName></definedNames><calcPr/></workbook>"#;
        let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let sheet1 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>10</v></c></row></sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f>Data!A1+1</f><v>11</v></c></row></sheetData></worksheet>"#;

        let original = crate::tests::make_zip(&[
            ("[Content_Types].xml", crate::tests::CT.as_bytes()),
            ("_rels/.rels", crate::tests::ROOT_RELS.as_bytes()),
            ("xl/workbook.xml", wb_xml.as_bytes()),
            ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet1.as_bytes()),
            ("xl/worksheets/sheet2.xml", sheet2.as_bytes()),
            ("xl/preserved.xml", b"<preserved>leave me alone</preserved>"),
        ]);

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().name = "Minerals".into();

        let mut formula_cell = wb.sheet(1).unwrap().cell(at("A1")).unwrap().clone();
        formula_cell.formula = Some(Formula::new("Minerals!A1+1").into());
        wb.sheet_mut(1).unwrap().set_cell(at("A1"), formula_cell);

        wb.names = vec![DefinedName { name: "Owned".into(), scope: None, formula: "Minerals!$A$1".into(), comment: String::new(), hidden: false }];

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::RenameSheet { sheet: 0 }]).unwrap();

        let before = parts(&original);
        let after = parts(&patched);
        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/workbook.xml", "xl/worksheets/sheet2.xml"]);
        assert_eq!(before["xl/preserved.xml"], after["xl/preserved.xml"]);
        assert_eq!(before["xl/worksheets/sheet1.xml"], after["xl/worksheets/sheet1.xml"]);

        let workbook = String::from_utf8(after["xl/workbook.xml"].clone()).unwrap();
        assert!(workbook.contains(r#"name="Minerals""#));
        assert!(!workbook.contains(r#"name="Data""#));
        assert!(workbook.contains(r#"<definedName name="Owned">Minerals!$A$1</definedName>"#));
        assert!(workbook.contains(r#"<definedName name="_xlnm.Print_Area" localSheetId="0">Minerals!$A$1:$B$2</definedName>"#));

        let sheet = String::from_utf8(after["xl/worksheets/sheet2.xml"].clone()).unwrap();
        assert!(sheet.contains("<f>Minerals!A1+1</f>"));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().name, "Minerals");
        assert_eq!(reopened.sheet(1).unwrap().cell(at("A1")).unwrap().formula.as_ref().unwrap().text, "Minerals!A1+1");
        assert_eq!(reopened.names.iter().find(|n| n.name == "Owned").unwrap().formula, "Minerals!$A$1");
        assert_eq!(reopened.sheet(0).unwrap().print.print_area.unwrap().a1(), "A1:B2");
    }

    #[test]
    fn structural_move_sheet_preserves_package_and_raw_sheet_indices() {
        let wb_xml = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><bookViews><workbookView activeTab="0" firstSheet="0"/></bookViews><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Chart" sheetId="2" r:id="rIdChart"/><sheet name="Calc" sheetId="3" r:id="rId2"/></sheets><definedNames><definedName name="DataLocal" localSheetId="0">Data!$A$1</definedName><definedName name="CalcLocal" localSheetId="2">Calc!$A$1</definedName><definedName name="_xlnm.Print_Area" localSheetId="0">Data!$A$1:$B$2</definedName></definedNames><customWorkbookViews><customWorkbookView name="Data View" guid="{00000000-0000-0000-0000-000000000001}" activeSheetId="0"/></customWorkbookViews><calcPr/></workbook>"#;
        let wb_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rIdChart" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let sheet1 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>10</v></c></row></sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>20</v></c></row></sheetData></worksheet>"#;
        let chart_sheet = r#"<chartsheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"/>"#;

        let original = crate::tests::make_zip(&[
            ("[Content_Types].xml", crate::tests::CT.as_bytes()),
            ("_rels/.rels", crate::tests::ROOT_RELS.as_bytes()),
            ("xl/workbook.xml", wb_xml.as_bytes()),
            ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet1.as_bytes()),
            ("xl/worksheets/sheet2.xml", sheet2.as_bytes()),
            ("xl/chartsheets/sheet1.xml", chart_sheet.as_bytes()),
            ("xl/preserved.xml", b"<preserved>leave me alone</preserved>"),
        ]);

        let (base, _) = read_xlsx(&original).unwrap();
        assert_eq!(base.sheets.len(), 2);
        assert_eq!(base.sheet(0).unwrap().name, "Data");
        assert_eq!(base.sheet(1).unwrap().name, "Calc");

        let mut wb = base.clone();

        let moved = wb.sheets.remove(0);
        wb.sheets.insert(1, moved);
        wb.active_sheet = 1;

        for name in &mut wb.names {
            if let Some(scope) = name.scope.as_mut() {
                *scope = match *scope {
                    0 => 1,
                    1 => 0,
                    x => x,
                };
            }
        }

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::MoveSheet { from: 0, to: 1 }]).unwrap();

        let before = parts(&original);
        let after = parts(&patched);
        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/workbook.xml"]);
        assert_eq!(before["xl/worksheets/sheet1.xml"], after["xl/worksheets/sheet1.xml"]);
        assert_eq!(before["xl/worksheets/sheet2.xml"], after["xl/worksheets/sheet2.xml"]);
        assert_eq!(before["xl/chartsheets/sheet1.xml"], after["xl/chartsheets/sheet1.xml"]);
        assert_eq!(before["xl/preserved.xml"], after["xl/preserved.xml"]);

        let workbook = String::from_utf8(after["xl/workbook.xml"].clone()).unwrap();

        let calc_pos = workbook.find(r#"name="Calc""#).unwrap();
        let chart_pos = workbook.find(r#"name="Chart""#).unwrap();
        let data_pos = workbook.find(r#"name="Data""#).unwrap();
        assert!(calc_pos < chart_pos && chart_pos < data_pos);

        assert!(workbook.contains(r#"activeTab="2""#));
        assert!(workbook.contains(r#"firstSheet="2""#));
        assert!(workbook.contains(r#"name="DataLocal" localSheetId="2""#));
        assert!(workbook.contains(r#"name="CalcLocal" localSheetId="0""#));
        assert!(workbook.contains(r#"name="_xlnm.Print_Area" localSheetId="2""#));
        assert!(workbook.contains(r#"activeSheetId="2""#));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().name, "Calc");
        assert_eq!(reopened.sheet(1).unwrap().name, "Data");
        assert_eq!(reopened.active_sheet, 1);
        assert_eq!(reopened.sheet(0).unwrap().cell(at("A1")).unwrap().value, Value::Number(20.0));
        assert_eq!(reopened.sheet(1).unwrap().cell(at("A1")).unwrap().value, Value::Number(10.0));
        assert_eq!(reopened.sheet(1).unwrap().print.print_area.unwrap().a1(), "A1:B2");

        let data_local = reopened.names.iter().find(|n| n.name == "DataLocal").unwrap();
        let calc_local = reopened.names.iter().find(|n| n.name == "CalcLocal").unwrap();
        assert_eq!(data_local.scope, Some(1));
        assert_eq!(calc_local.scope, Some(0));
    }

    #[test]
    fn structural_insert_column_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_cols(1, 1);

        let patched =
            patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::InsertColumns { sheet: 0, at: 1, count: 1 }]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(sheet.contains(r#"<c r="A1"><v>1</v></c>"#));
        assert!(sheet.contains(r#"<c r="C1"><v>2</v></c>"#));
        assert_eq!(parts(&original)["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("C1")).unwrap().value, Value::Number(2.0));
    }

    #[test]
    fn structural_delete_column_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.take_range(gridcraft_core::RangeRef::cols(1, 1));
        wb.sheet_mut(0).unwrap().cells.shift_cols(2, -1);

        let patched =
            patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::DeleteColumns { sheet: 0, at: 1, count: 1 }]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(sheet.contains(r#"<c r="A1"><v>1</v></c>"#));
        assert!(sheet.contains(r#"<c r="B1"><v>3</v></c>"#));
        assert!(!sheet.contains(r#"<v>2</v>"#));
        assert_eq!(parts(&original)["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("B1")).unwrap().value, Value::Number(3.0));
    }

    #[test]
    fn structural_delete_row_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="A2"><v>2</v></c></row><row r="3"><c r="A3"><v>3</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.take_range(gridcraft_core::RangeRef::rows(1, 1));
        wb.sheet_mut(0).unwrap().cells.shift_rows(2, -1);

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::DeleteRows { sheet: 0, at: 1, count: 1 }]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(sheet.contains(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#));
        assert!(sheet.contains(r#"<row r="2"><c r="A2"><v>3</v></c></row>"#));
        assert!(!sheet.contains(r#"<v>2</v>"#));
        assert_eq!(parts(&original)["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("A2")).unwrap().value, Value::Number(3.0));
    }

    #[test]
    fn structural_insert_cells_right_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c></row><row r="2"><c r="A2"><v>4</v></c><c r="B2"><v>5</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_cols_in_rows(0, 0, 1, 1);

        let patched = patch_xlsx_structural(
            &original,
            &base,
            &wb,
            &[],
            &[crate::StructuralEdit::InsertCellsRight { sheet: 0, start_row: 0, end_row: 0, at_col: 1, count: 1 }],
        )
        .unwrap();

        assert_only_sheet_changed(&original, &patched);
        assert_eq!(parts(&original)["xl/preserved.xml"], parts(&patched)["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let sh = reopened.sheet(0).unwrap();
        assert_eq!(sh.value(at("A1")), Value::Number(1.0));
        assert_eq!(sh.value(at("C1")), Value::Number(2.0));
        assert_eq!(sh.value(at("D1")), Value::Number(3.0));
        assert_eq!(sh.value(at("B2")), Value::Number(5.0));
    }

    #[test]
    fn structural_insert_cells_right_reuses_shared_string_without_rewriting_sst() {
        let shared_strings = r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1"><si><t>Owned</t></si></sst>"#;
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1" t="s"><v>0</v></c><c r="C1"><v>2</v></c></row></sheetData>"#,
            &[("xl/sharedStrings.xml", shared_strings), ("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#,
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_cols_in_rows(0, 0, 1, 1);

        let patched = patch_xlsx_structural(
            &original,
            &base,
            &wb,
            &[],
            &[crate::StructuralEdit::InsertCellsRight { sheet: 0, start_row: 0, end_row: 0, at_col: 1, count: 1 }],
        )
        .unwrap();

        assert_only_sheet_changed(&original, &patched);
        assert_eq!(parts(&original)["xl/sharedStrings.xml"], parts(&patched)["xl/sharedStrings.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let sh = reopened.sheet(0).unwrap();
        assert_eq!(sh.value(at("A1")), Value::Number(1.0));
        assert_eq!(sh.value(at("B1")), Value::Empty);
        assert_eq!(sh.value(at("C1")), Value::Text("Owned".into()));
        assert_eq!(sh.value(at("D1")), Value::Number(2.0));
    }

    #[test]
    fn structural_delete_cells_left_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c></row><row r="2"><c r="A2"><v>4</v></c><c r="B2"><v>5</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().take_cells(gridcraft_core::RangeRef::new(at("B1"), at("B1")));
        wb.sheet_mut(0).unwrap().cells.shift_cols_in_rows(0, 0, 2, -1);

        let patched = patch_xlsx_structural(
            &original,
            &base,
            &wb,
            &[],
            &[crate::StructuralEdit::DeleteCellsLeft { sheet: 0, start_row: 0, end_row: 0, at_col: 1, count: 1 }],
        )
        .unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let sh = reopened.sheet(0).unwrap();
        assert_eq!(sh.value(at("A1")), Value::Number(1.0));
        assert_eq!(sh.value(at("B1")), Value::Number(3.0));
        assert_eq!(sh.value(at("C1")), Value::Empty);
        assert_eq!(sh.value(at("B2")), Value::Number(5.0));
    }

    #[test]
    fn structural_insert_cells_down_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row><row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row><row r="3"><c r="A3"><v>5</v></c><c r="B3"><v>6</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_rows_in_cols(0, 0, 1, 1);

        let patched = patch_xlsx_structural(
            &original,
            &base,
            &wb,
            &[],
            &[crate::StructuralEdit::InsertCellsDown { sheet: 0, start_col: 0, end_col: 0, at_row: 1, count: 1 }],
        )
        .unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let sh = reopened.sheet(0).unwrap();
        assert_eq!(sh.value(at("A1")), Value::Number(1.0));
        assert_eq!(sh.value(at("A2")), Value::Empty);
        assert_eq!(sh.value(at("A3")), Value::Number(3.0));
        assert_eq!(sh.value(at("A4")), Value::Number(5.0));
        assert_eq!(sh.value(at("B2")), Value::Number(4.0));
    }

    #[test]
    fn structural_delete_cells_up_preserves_package() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row><row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row><row r="3"><c r="A3"><v>5</v></c><c r="B3"><v>6</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().take_cells(gridcraft_core::RangeRef::new(at("A2"), at("A2")));
        wb.sheet_mut(0).unwrap().cells.shift_rows_in_cols(0, 0, 2, -1);

        let patched = patch_xlsx_structural(
            &original,
            &base,
            &wb,
            &[],
            &[crate::StructuralEdit::DeleteCellsUp { sheet: 0, start_col: 0, end_col: 0, at_row: 1, count: 1 }],
        )
        .unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let sh = reopened.sheet(0).unwrap();
        assert_eq!(sh.value(at("A1")), Value::Number(1.0));
        assert_eq!(sh.value(at("A2")), Value::Number(5.0));
        assert_eq!(sh.value(at("A3")), Value::Empty);
        assert_eq!(sh.value(at("B2")), Value::Number(4.0));
    }

    #[test]
    fn structural_insert_row_patches_formula_on_other_sheet() {
        let wb_xml = r#"<?xml version="1.0"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/><sheet name="Calc" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let wb_rels = r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let sheet1 = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
        let sheet2 = r#"<?xml version="1.0"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1"><f>Data!A1+1</f><v>2</v></c></row></sheetData></worksheet>"#;

        let original = crate::tests::make_zip(&[
            ("[Content_Types].xml", crate::tests::CT.as_bytes()),
            ("_rels/.rels", crate::tests::ROOT_RELS.as_bytes()),
            ("xl/workbook.xml", wb_xml.as_bytes()),
            ("xl/_rels/workbook.xml.rels", wb_rels.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet1.as_bytes()),
            ("xl/worksheets/sheet2.xml", sheet2.as_bytes()),
            ("xl/preserved.xml", b"<preserved>leave me alone</preserved>"),
        ]);

        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_rows(0, 1);

        let mut formula_cell = wb.sheet(1).unwrap().cell(at("A1")).unwrap().clone();
        formula_cell.formula = Some(Formula::new("Data!A2+1").into());
        wb.sheet_mut(1).unwrap().set_cell(at("A1"), formula_cell);

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::InsertRows { sheet: 0, at: 0, count: 1 }]).unwrap();

        let before = parts(&original);
        let after = parts(&patched);

        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/worksheets/sheet1.xml", "xl/worksheets/sheet2.xml"]);
        assert_eq!(before["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("A2")).unwrap().value, Value::Number(1.0));
        assert_eq!(reopened.sheet(1).unwrap().cell(at("A1")).unwrap().formula.as_ref().unwrap().text, "Data!A2+1");
    }

    #[test]
    fn structural_insert_row_patches_rewritten_formula() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_rows(0, 1);

        let mut moved = wb.sheet(0).unwrap().cell(at("B2")).unwrap().clone();
        moved.formula = Some(Formula::new("A2+1").into());
        wb.sheet_mut(0).unwrap().set_cell(at("B2"), moved);

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::InsertRows { sheet: 0, at: 0, count: 1 }]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(sheet.contains(r#"<c r="B2"><f>A2+1</f><v>2</v></c>"#));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("B2")).unwrap().formula.as_ref().unwrap().text, "A2+1");
    }

    #[test]
    fn structural_insert_row_preserves_package() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().cells.shift_rows(0, 1);

        let patched = patch_xlsx_structural(&original, &base, &wb, &[], &[crate::StructuralEdit::InsertRows { sheet: 0, at: 0, count: 1 }]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(sheet.contains(r#"<row r="2"><c r="A2"><v>1</v></c><c r="B2"><f>A1+1</f><v>2</v></c></row>"#));
        assert_eq!(parts(&original)["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().cell(at("A2")).unwrap().value, Value::Number(1.0));
    }

    #[test]
    fn structural_coordinate_shift_insert_rows() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="B2"><v>2</v></c></row></sheetData></worksheet>"#;

        let shifted = shift_sheet_data_coordinates(xml, crate::StructuralEdit::InsertRows { sheet: 0, at: 1, count: 2 }).unwrap();

        assert!(shifted.contains(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#));
        assert!(shifted.contains(r#"<row r="4"><c r="B4"><v>2</v></c></row>"#));
        assert!(!shifted.contains(r#"r="2""#));
    }

    #[test]
    fn structural_coordinate_shift_delete_rows() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row><row r="2"><c r="B2"><v>2</v></c></row><row r="3"><c r="C3"><v>3</v></c></row></sheetData></worksheet>"#;

        let shifted = shift_sheet_data_coordinates(xml, crate::StructuralEdit::DeleteRows { sheet: 0, at: 1, count: 1 }).unwrap();

        assert!(shifted.contains(r#"<row r="1"><c r="A1"><v>1</v></c></row>"#));
        assert!(!shifted.contains(r#"<v>2</v>"#));
        assert!(shifted.contains(r#"<row r="2"><c r="C2"><v>3</v></c></row>"#));
    }

    #[test]
    fn structural_coordinate_shift_insert_columns() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row></sheetData></worksheet>"#;

        let shifted = shift_sheet_data_coordinates(xml, crate::StructuralEdit::InsertColumns { sheet: 0, at: 1, count: 2 }).unwrap();

        assert!(shifted.contains(r#"<c r="A1"><v>1</v></c>"#));
        assert!(shifted.contains(r#"<c r="D1"><v>2</v></c>"#));
    }

    #[test]
    fn structural_coordinate_shift_delete_columns() {
        let xml =
            r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c></row></sheetData></worksheet>"#;

        let shifted = shift_sheet_data_coordinates(xml, crate::StructuralEdit::DeleteColumns { sheet: 0, at: 1, count: 1 }).unwrap();

        assert!(shifted.contains(r#"<c r="A1"><v>1</v></c>"#));
        assert!(!shifted.contains(r#"<v>2</v>"#));
        assert!(shifted.contains(r#"<c r="B1"><v>3</v></c>"#));
    }

    fn fixture() -> Vec<u8> {
        crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c><c r="B1"><f>A1+1</f><v>2</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            "",
            "",
        )
    }

    fn styled_fixture() -> Vec<u8> {
        let styles = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font/><font><b/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="1"/></cellXfs></styleSheet>"#;
        let rels =
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#;

        crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1" s="1"><v>1</v></c><c r="B1"><f>A1+1</f><v>2</v></c></row></sheetData>"#,
            &[("xl/styles.xml", styles), ("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            rels,
            "",
        )
    }

    #[test]
    fn existing_style_change_only_updates_cell_style_attribute() {
        let original = styled_fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        let bold = wb.sheet(0).unwrap().style_id(at("A1"));
        let formula = wb.sheet(0).unwrap().cell(at("B1")).unwrap().clone();

        wb.sheet_mut(0).unwrap().set_cell(at("B1"), Cell { style: bold, ..formula });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("B1"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let before = parts(&original);
        let after = parts(&patched);
        let before_sheet = String::from_utf8(before["xl/worksheets/sheet1.xml"].clone()).unwrap();
        let after_sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();

        assert!(before_sheet.contains(r#"<c r="B1"><f>A1+1</f><v>2</v></c>"#));
        assert!(after_sheet.contains(r#"<c r="B1" s="1"><f>A1+1</f><v>2</v></c>"#));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().style_id(at("B1")), reopened.sheet(0).unwrap().style_id(at("A1")));
    }

    #[test]
    fn new_style_appends_to_styles_and_updates_cell() {
        let original = styled_fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        let current = wb.sheet(0).unwrap().style_id(at("B1"));
        let italic = wb.styles.derive(current, |style| {
            style.font.italic = true;
        });

        let formula = wb.sheet(0).unwrap().cell(at("B1")).unwrap().clone();
        wb.sheet_mut(0).unwrap().set_cell(at("B1"), Cell { style: italic, ..formula });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("B1"))]).unwrap();

        let before = parts(&original);
        let after = parts(&patched);

        assert_eq!(before.keys().collect::<Vec<_>>(), after.keys().collect::<Vec<_>>());

        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/styles.xml", "xl/worksheets/sheet1.xml"]);
        assert_eq!(before["xl/preserved.xml"], after["xl/preserved.xml"]);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let style_id = reopened.sheet(0).unwrap().style_id(at("B1"));
        assert!(reopened.styles.get(style_id).font.italic);

        let cell = reopened.sheet(0).unwrap().cell(at("B1")).unwrap();
        assert_eq!(cell.formula.as_ref().unwrap().text, "A1+1");
        assert_eq!(cell.value, Value::Number(2.0));
    }

    #[test]
    fn new_custom_number_format_appends_to_styles() {
        let original = styled_fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        let current = wb.sheet(0).unwrap().style_id(at("A1"));
        let custom = wb.styles.derive(current, |style| {
            style.num_fmt = NumFmt::new(r#"0.0000 "nma""#);
        });

        let cell = wb.sheet(0).unwrap().cell(at("A1")).unwrap().clone();
        wb.sheet_mut(0).unwrap().set_cell(at("A1"), Cell { style: custom, ..cell });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("A1"))]).unwrap();

        let before = parts(&original);
        let after = parts(&patched);

        let changed: Vec<_> = before.iter().filter_map(|(name, data)| (after.get(name) != Some(data)).then_some(name.as_str())).collect();

        assert_eq!(changed, vec!["xl/styles.xml", "xl/worksheets/sheet1.xml"]);
        assert_eq!(before["xl/preserved.xml"], after["xl/preserved.xml"]);

        let styles = String::from_utf8(after["xl/styles.xml"].clone()).unwrap();
        assert!(styles.contains(r#"formatCode="0.0000 &quot;nma&quot;""#));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let style_id = reopened.sheet(0).unwrap().style_id(at("A1"));
        assert_eq!(reopened.styles.get(style_id).num_fmt.as_str(), r#"0.0000 "nma""#);
    }

    #[test]
    fn constant_edit_changes_only_its_worksheet_part() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().set_value(at("A1"), Value::Number(9.0));

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("A1"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().value(at("A1")), Value::Number(9.0));
    }

    #[test]
    fn new_constant_cell_in_existing_row_changes_only_worksheet() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().set_value(at("C1"), Value::Number(42.0));

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("C1"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().value(at("C1")), Value::Number(42.0));
    }

    #[test]
    fn new_constant_cell_preserves_inherited_row_style() {
        let styles = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font/><font><b/><name val="Arial"/><sz val="10"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="1"/></cellXfs></styleSheet>"#;
        let rels =
            r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#;

        let original = crate::tests::minimal(
            r#"<sheetData><row r="1" s="1" customFormat="1"><c r="A1"><v>1</v></c></row></sheetData>"#,
            &[("xl/styles.xml", styles), ("xl/preserved.xml", "<preserved>leave me alone</preserved>")],
            rels,
            "",
        );

        let (base, _) = read_xlsx(&original).unwrap();
        let inherited = base.sheet(0).unwrap().style_id(at("C1"));
        assert_ne!(inherited.0, 0);

        let mut wb = base.clone();
        wb.sheet_mut(0).unwrap().set_value(at("C1"), Value::Number(42.0));

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("C1"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let after = parts(&patched);
        let sheet = String::from_utf8(after["xl/worksheets/sheet1.xml"].clone()).unwrap();
        assert!(sheet.contains(r#"<c r="C1" s="1"><v>42</v></c>"#));

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().style_id(at("C1")), inherited);
        assert_eq!(reopened.sheet(0).unwrap().value(at("C1")), Value::Number(42.0));
    }

    #[test]
    fn new_constant_cell_can_create_new_row() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().set_value(at("A3"), Value::Text("new row".into()));

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("A3"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        assert_eq!(reopened.sheet(0).unwrap().value(at("A3")), Value::Text("new row".into()));
    }

    #[test]
    fn constant_cell_can_become_formula() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        let style = wb.sheet(0).unwrap().style_id(at("A1"));
        wb.sheet_mut(0).unwrap().set_cell(at("A1"), Cell { value: Value::Number(4.0), formula: Some(Arc::new(Formula::new("=2+2"))), style });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("A1"))]).unwrap();
        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let cell = reopened.sheet(0).unwrap().cell(at("A1")).unwrap();

        assert_eq!(cell.formula.as_ref().unwrap().text, "2+2");
        assert_eq!(cell.value, Value::Number(4.0));
    }

    #[test]
    fn formula_cell_can_become_constant() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0).unwrap().set_value(at("B1"), Value::Number(7.0));

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("B1"))]).unwrap();
        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let cell = reopened.sheet(0).unwrap().cell(at("B1")).unwrap();

        assert!(cell.formula.is_none());
        assert_eq!(cell.value, Value::Number(7.0));
    }

    #[test]
    fn new_cell_can_be_formula() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        wb.sheet_mut(0)
            .unwrap()
            .set_cell(at("C1"), Cell { value: Value::Number(2.0), formula: Some(Arc::new(Formula::new("=A1+1"))), style: Default::default() });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("C1"))]).unwrap();
        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let cell = reopened.sheet(0).unwrap().cell(at("C1")).unwrap();

        assert_eq!(cell.formula.as_ref().unwrap().text, "A1+1");
        assert_eq!(cell.value, Value::Number(2.0));
    }

    #[test]
    fn ordinary_formula_edit_changes_only_its_worksheet_part() {
        let original = fixture();
        let (base, _) = read_xlsx(&original).unwrap();
        let mut wb = base.clone();

        let sheet = wb.sheet_mut(0).unwrap();
        let style = sheet.style_id(at("B1"));
        sheet.set_cell(at("B1"), Cell { value: Value::Number(3.0), formula: Some(Arc::new(Formula::new("=A1+2"))), style });

        let patched = patch_xlsx(&original, &base, &wb, &[(0, at("B1"))]).unwrap();

        assert_only_sheet_changed(&original, &patched);

        let (reopened, _) = read_xlsx(&patched).unwrap();
        let cell = reopened.sheet(0).unwrap().cell(at("B1")).unwrap();

        assert_eq!(cell.formula.as_ref().unwrap().text, "A1+2");
        assert_eq!(cell.value, Value::Number(3.0));
    }
}


#[cfg(test)]
mod shared_formula_edit_tests {
    use super::*;
    use std::sync::Arc;
    use gridcraft_model::{Cell, Formula, Sheet};

    fn formula_at(sheet: &mut Sheet, address: &str, formula: &str, value: f64) {
        let cell_ref = CellRef::parse(address).unwrap();
        sheet.set_cell(cell_ref, Cell {
            value: Value::Number(value),
            formula: Some(Arc::new(Formula::new(formula))),
            style: Default::default(),
        });
    }

    #[test]
    fn replacing_shared_formula_master_expands_children_without_losing_cache() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="C1"><f t="shared" si="17" ref="C1:C2">A1*2</f><v>4</v></c></row><row r="2"><c r="C2"><f t="shared" si="17"/><v>6</v></c></row></sheetData></worksheet>"#;
        let mut before = Sheet::new("Example");
        formula_at(&mut before, "C1", "A1*2", 4.0);
        formula_at(&mut before, "C2", "A2*2", 6.0);
        let mut after = before.clone();
        after.set_value(CellRef::parse("C1").unwrap(), Value::Number(19.0));
        let dirty = BTreeSet::from([CellRef::parse("C1").unwrap()]);

        let result = expand_edited_shared_formula_groups(xml, &before, &after, &dirty).unwrap();
        assert!(!result.contains("t=\"shared\""));
        assert!(result.contains(r#"<c r="C1"><f>A1*2</f><v>4</v></c>"#));
        assert!(result.contains(r#"<c r="C2"><f>A2*2</f><v>6</v></c>"#));
    }

    #[test]
    fn only_style_edit_does_not_rewrite_original_shared_formulas() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="C1"><f t="shared" si="17">A1*2</f><v>4</v></c></row></sheetData></worksheet>"#;
        let mut before = Sheet::new("Example");
        formula_at(&mut before, "C1", "A1*2", 4.0);
        let after = before.clone();
        let dirty = BTreeSet::from([CellRef::parse("C1").unwrap()]);

        assert_eq!(expand_edited_shared_formula_groups(xml, &before, &after, &dirty).unwrap(), xml);
    }
}


#[cfg(test)]
mod exact_xf_copy_tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn copy_preserves_source_xf_index_when_two_xfs_parse_to_same_style() {
        // The source uses XF #2. XF #1 has the same modeled Style, so the
        // normalized style lookup alone would incorrectly choose XF #1.
        let styles = r#"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font/><font><b/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border/></borders><cellXfs count="3"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="1"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" applyFont="1"/></cellXfs></styleSheet>"#;
        let rels = r#"<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#;
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1" s="2"><v>1</v></c><c r="B1"><v>2</v></c></row></sheetData>"#,
            &[("xl/styles.xml", styles), ("xl/preserved.xml", "<preserved>keep</preserved>")],
            rels,
            "",
        );

        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut edited = base.clone();
        let a1 = CellRef::parse("A1").unwrap();
        let b1 = CellRef::parse("B1").unwrap();
        let source = base.sheet(0).unwrap().cell(a1).unwrap().clone();
        edited.sheet_mut(0).unwrap().set_cell(b1, source);

        let output = patch_xlsx_with_style_copies(&original, &base, &edited, &[(0, b1)], &[(0, a1, 0, b1)]).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(&output)).unwrap();
        let mut sheet_xml = String::new();
        zip.by_name("xl/worksheets/sheet1.xml").unwrap().read_to_string(&mut sheet_xml).unwrap();
        assert!(sheet_xml.contains(r#"<c r="B1" s="2"><v>1</v></c>"#), "{sheet_xml}");
        let mut preserved = String::new();
        zip.by_name("xl/preserved.xml").unwrap().read_to_string(&mut preserved).unwrap();
        assert_eq!(preserved, "<preserved>keep</preserved>");
    }

    #[test]
    fn no_style_copy_hint_keeps_normal_style_resolution() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>"#,
            &[("xl/preserved.xml", "<preserved>keep</preserved>")],
            "",
            "",
        );
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut edited = base.clone();
        let b1 = CellRef::parse("B1").unwrap();
        edited.sheet_mut(0).unwrap().set_value(b1, Value::Number(5.0));
        let baseline = patch_xlsx(&original, &base, &edited, &[(0, b1)]).unwrap();
        let hinted = patch_xlsx_with_style_copies(&original, &base, &edited, &[(0, b1)], &[]).unwrap();
        assert_eq!(baseline, hinted);
    }
}


#[cfg(test)]
mod merge_preservation_tests {
    use super::*;
    use std::io::Read;

    fn read_part(bytes: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut out = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn merged_area_copy_updates_merge_cells_and_no_other_package_part() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>5</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A1:B1"/></mergeCells>"#,
            &[("xl/preserved.xml", "<preserved>vba and other parts</preserved>")],
            "",
            "",
        );
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut edited = base.clone();
        edited.sheet_mut(0).unwrap().merges.push(gridcraft_core::RangeRef::parse("C4:D4").unwrap());

        let patched = patch_xlsx(&original, &base, &edited, &[]).unwrap();
        let xml = read_part(&patched, "xl/worksheets/sheet1.xml");
        assert!(xml.contains(r#"<mergeCells count="2">"#));
        assert!(xml.contains(r#"<mergeCell ref="A1:B1"/>"#));
        assert!(xml.contains(r#"<mergeCell ref="C4:D4"/>"#));
        assert_eq!(
            read_part(&patched, "xl/preserved.xml"),
            read_part(&original, "xl/preserved.xml"),
        );
        let (roundtrip, _) = crate::read_xlsx(&patched).unwrap();
        assert!(roundtrip.sheet(0).unwrap().merges.contains(&gridcraft_core::RangeRef::parse("C4:D4").unwrap()));
    }

    #[test]
    fn first_merge_is_inserted_before_page_margins() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData><pageMargins left="0.3" right="0.3" top="0.2" bottom="0.2" header="0" footer="0"/>"#,
            &[],
            "",
            "",
        );
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut edited = base.clone();
        edited.sheet_mut(0).unwrap().merges.push(gridcraft_core::RangeRef::parse("A1:B1").unwrap());
        let patched = patch_xlsx(&original, &base, &edited, &[]).unwrap();
        let xml = read_part(&patched, "xl/worksheets/sheet1.xml");
        assert!(xml.find("<mergeCells").unwrap() < xml.find("<pageMargins").unwrap());
        assert!(xml.contains(r#"<mergeCell ref="A1:B1"/>"#));
    }

    #[test]
    fn overlapping_merges_are_rejected_instead_of_written() {
        let original = crate::tests::minimal(r#"<sheetData/>"#, &[], "", "");
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut edited = base.clone();
        edited.sheet_mut(0).unwrap().merges.push(gridcraft_core::RangeRef::parse("A1:C1").unwrap());
        edited.sheet_mut(0).unwrap().merges.push(gridcraft_core::RangeRef::parse("B1:D1").unwrap());
        let err = patch_xlsx(&original, &base, &edited, &[]).unwrap_err().to_string();
        assert!(err.contains("overlapping merged ranges"), "{err}");
    }
}


#[cfg(test)]
mod row_metadata_patch_tests {
    use super::*;
    use std::io::Read;

    fn sheet_xml(package: &[u8]) -> String {
        let mut archive = zip::ZipArchive::new(Cursor::new(package)).unwrap();
        let mut s = String::new();
        archive.by_name("xl/worksheets/sheet1.xml").unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn new_formatted_row_keeps_autofitted_height_after_save() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>"#,
            &[("xl/opaque.xml", "<opaque>preserved</opaque>")],
            "", "",
        );
        let (before, _) = crate::read_xlsx(&original).unwrap();
        let mut after = before.clone();
        after.sheet_mut(0).unwrap().rows.insert(
            4,
            gridcraft_model::LineInfo {
                size: Some(45.5),
                ..Default::default()
            },
        );
        let patched = patch_xlsx(&original, &before, &after, &[]).unwrap();
        let sheet = sheet_xml(&patched);
        assert!(sheet.contains(r#"<row r="5" ht="45.5" customHeight="1"/>"#), "{sheet}");
        let mut archive = zip::ZipArchive::new(Cursor::new(&patched)).unwrap();
        let mut opaque = String::new();
        archive.by_name("xl/opaque.xml").unwrap().read_to_string(&mut opaque).unwrap();
        assert_eq!(opaque, "<opaque>preserved</opaque>");
    }

    #[test]
    fn changing_existing_row_height_keeps_its_cell_contents() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1" ht="18" customHeight="1"><c r="A1"><v>7</v></c></row></sheetData>"#,
            &[], "", "",
        );
        let (before, _) = crate::read_xlsx(&original).unwrap();
        let mut after = before.clone();
        after.sheet_mut(0).unwrap().rows.insert(
            0,
            gridcraft_model::LineInfo {size:Some(24.0), hidden:true, ..Default::default()}
        );
        let patched = patch_xlsx(&original, &before, &after, &[]).unwrap();
        let sheet = sheet_xml(&patched);
        assert!(sheet.contains(r#"ht="24""#), "{sheet}");
        assert!(sheet.contains(r#"hidden="1""#), "{sheet}");
        assert!(sheet.contains(r#"<c r="A1"><v>7</v></c>"#), "{sheet}");
    }

    #[test]
    fn row_height_can_be_added_to_empty_sheetdata() {
        let original = crate::tests::minimal(r#"<sheetData/>"#, &[], "", "");
        let (before, _) = crate::read_xlsx(&original).unwrap();
        let mut after = before.clone();
        after.sheet_mut(0).unwrap().rows.insert(
            2,
            gridcraft_model::LineInfo {size:Some(30.0), ..Default::default()},
        );
        let patched = patch_xlsx(&original, &before, &after, &[]).unwrap();
        let sheet = sheet_xml(&patched);
        assert!(sheet.contains(r#"<sheetData><row r="3" ht="30" customHeight="1"/></sheetData>"#), "{sheet}");
    }
}


#[cfg(test)]
mod cell_removal_roundtrip_tests {
    use super::*;
    use std::io::Read;

    fn worksheet(package: &[u8]) -> String {
        let mut z = zip::ZipArchive::new(Cursor::new(package)).unwrap();
        let mut xml = String::new();
        z.by_name("xl/worksheets/sheet1.xml").unwrap().read_to_string(&mut xml).unwrap();
        xml
    }

    #[test]
    fn deleting_last_cell_removes_synthetic_row_instead_of_leaving_empty_style() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>7</v></c></row></sheetData>"#,
            &[("xl/opaque.xml", "<opaque>preserve</opaque>")], "", "",
        );
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut after = base.clone();
        after.sheet_mut(0).unwrap().remove_cell(CellRef::parse("A1").unwrap());
        let patched = patch_xlsx(&original, &base, &after, &[(0, CellRef::parse("A1").unwrap())]).unwrap();
        let xml = worksheet(&patched);
        assert!(!xml.contains("A1"), "{xml}");
        assert!(!xml.contains("<row r=\"1\""), "{xml}");
        let (roundtrip, _) = crate::read_xlsx(&patched).unwrap();
        assert!(roundtrip.sheet(0).unwrap().cell(CellRef::parse("A1").unwrap()).is_none());
    }

    #[test]
    fn removing_one_of_two_cells_keeps_other_cell_and_row_intact() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>7</v></c><c r="B1"><v>8</v></c></row></sheetData>"#,
            &[], "", "",
        );
        let (base, _) = crate::read_xlsx(&original).unwrap();
        let mut after = base.clone();
        after.sheet_mut(0).unwrap().remove_cell(CellRef::parse("A1").unwrap());
        let patched = patch_xlsx(&original, &base, &after, &[(0, CellRef::parse("A1").unwrap())]).unwrap();
        let xml = worksheet(&patched);
        assert!(!xml.contains("<c r=\"A1\""), "{xml}");
        assert!(xml.contains(r#"<c r="B1"><v>8</v></c>"#), "{xml}");
        assert!(xml.contains(r#"<row r="1">"#), "{xml}");
    }

    #[test]
    fn unmodeled_xml_cell_is_preserved_without_model_change() {
        let original = crate::tests::minimal(
            r#"<sheetData><row r="1"><c r="A1"><v>7</v></c></row></sheetData>"#,
            &[], "", "",
        );
        let (mut base, _) = crate::read_xlsx(&original).unwrap();
        base.sheet_mut(0).unwrap().remove_cell(CellRef::parse("A1").unwrap());
        let after = base.clone();
        // The imported model is missing this opaque XML cell, but the
        // destination does not request its removal. Keep the original part.
        let patched = patch_xlsx(&original, &base, &after, &[(0, CellRef::parse("A1").unwrap())]).unwrap();
        assert_eq!(worksheet(&patched), worksheet(&original));
    }
}
