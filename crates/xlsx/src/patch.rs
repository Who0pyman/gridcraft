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
    count_changed: bool,
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

        Ok(Self { part, map, next_index: index, new_strings: Vec::new(), count_changed: false })
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
        self.count_changed || !self.new_strings.is_empty()
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

fn apply_structural_edit_to_baseline(wb: &mut Workbook, edit: crate::StructuralEdit) -> Result<(), IoError> {
    let (sheet_index, at, count) = match edit {
        crate::StructuralEdit::InsertRows { sheet, at, count }
        | crate::StructuralEdit::DeleteRows { sheet, at, count }
        | crate::StructuralEdit::InsertColumns { sheet, at, count }
        | crate::StructuralEdit::DeleteColumns { sheet, at, count } => (sheet, at, count),
    };

    if count == 0 {
        return Ok(());
    }

    let sheet = wb.sheet_mut(sheet_index).ok_or_else(|| IoError::Format(format!("structural edit refers to missing sheet {sheet_index}")))?;

    match edit {
        crate::StructuralEdit::InsertRows { .. } => {
            sheet.cells.shift_rows(at, count as i64);
        }
        crate::StructuralEdit::DeleteRows { .. } => {
            let end = at.saturating_add(count).saturating_sub(1).min(gridcraft_core::MAX_ROWS - 1);
            sheet.cells.take_range(gridcraft_core::RangeRef::rows(at, end));
            sheet.cells.shift_rows(at.saturating_add(count), -(count as i64));
        }
        crate::StructuralEdit::InsertColumns { .. } => {
            sheet.cells.shift_cols(at, count as i64);
        }
        crate::StructuralEdit::DeleteColumns { .. } => {
            let end = at.saturating_add(count).saturating_sub(1).min(gridcraft_core::MAX_COLS - 1);
            sheet.cells.take_range(gridcraft_core::RangeRef::cols(at, end));
            sheet.cells.shift_cols(at.saturating_add(count), -(count as i64));
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
            | crate::StructuralEdit::DeleteColumns { sheet, .. } => sheet,
        };

        let sheet = base.sheet(sheet_index).ok_or_else(|| IoError::Format(format!("structural edit refers to missing base sheet {sheet_index}")))?;

        if !structural_sheet_is_plain(sheet) {
            return Err(IoError::Format(format!(
                "safe structural XLSX patching does not yet support row/column edits on sheets with structural features: {}",
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
            text = shift_sheet_data_coordinates(&text, edit)?;
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

    if base.sheets.len() != wb.sheets.len() || base.sheets.iter().zip(&wb.sheets).any(|(a, b)| a.name != b.name) {
        return Err(IoError::Format("safe structural XLSX patching requires the worksheet set to remain unchanged".into()));
    }

    if base.names != wb.names {
        return Err(IoError::Format("safe structural XLSX patching does not yet support structural edits that change defined names".into()));
    }

    let structurally_shifted = structurally_shift_package(original, base, edits)?;

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

pub fn patch_xlsx(original: &[u8], base: &Workbook, wb: &Workbook, dirty_cells: &[(usize, CellRef)]) -> Result<Vec<u8>, IoError> {
    if dirty_cells.is_empty() {
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

    let sst_part = wb_rels.iter().find(|r| r.kind == "sharedStrings").map(|r| r.target.clone());

    let mut sst = PatchSst::load(&mut pkg, sst_part)?;

    let styles_part = wb_rels.iter().find(|r| r.kind == "styles").map(|r| r.target.clone());
    let mut styles = PatchStyles::load(&mut pkg, styles_part)?;

    let mut dirty_by_sheet: BTreeMap<usize, BTreeSet<CellRef>> = BTreeMap::new();
    for &(sheet_index, cell_ref) in dirty_cells {
        dirty_by_sheet.entry(sheet_index).or_default().insert(cell_ref);
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

        let mut text = String::from_utf8(bytes).map_err(|_| IoError::Xml(format!("{part} is not UTF-8 XML")))?;

        struct Edit {
            start: usize,
            end: usize,
            replacement: String,
        }

        let mut edits = Vec::new();
        let mut new_cells = Vec::new();

        for cell_ref in refs {
            let old_style = base_sheet.style_id(cell_ref);
            let new_style = sheet.style_id(cell_ref);

            let style_index = if old_style != new_style {
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

                if let Some(style_index) = style_index {
                    replacement = set_cell_style(&replacement, style_index)?;
                }

                if new_shared {
                    sst.count_changed = true;
                }

                new_cells.push((cell_ref, replacement));
                continue;
            };

            let original_cell = &text[start..end];

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

            if old_shared != new_shared {
                sst.count_changed = true;
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
    use gridcraft_model::{Cell, Formula, NumFmt};

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
