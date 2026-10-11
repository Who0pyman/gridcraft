use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Cursor, Read};

use gridcraft_core::{CellRef, Value};
use gridcraft_model::Workbook;

use crate::package::{Package, ZipOut, normalize};
use crate::{IoError, xml};

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

        for cell_ref in refs {
            let old_style = base_sheet.style_id(cell_ref);
            let new_style = sheet.style_id(cell_ref);
            if old_style != new_style {
                return Err(IoError::Format(format!("safe XLSX patching does not yet support style changes at {}!{}", sheet.name, cell_ref.a1())));
            }

            let old_cell = base_sheet.cell(cell_ref);
            let new_cell = sheet.cell(cell_ref);

            if old_cell == new_cell {
                continue;
            }

            let old_formula = old_cell.and_then(|c| c.formula.as_ref());
            let new_formula = new_cell.and_then(|c| c.formula.as_ref());

            if old_formula != new_formula {
                return Err(IoError::Format(format!("safe XLSX patching does not yet support formula edits at {}!{}", sheet.name, cell_ref.a1())));
            }

            // A dependent formula may have received a new cached result after recalculation.
            // Preserve the original formula cell exactly; Excel can recalculate it.
            if old_formula.is_some() {
                continue;
            }

            let a1 = cell_ref.a1();
            let Some((start, end, start_tag_end)) = find_cell_span(&text, &a1) else {
                return Err(IoError::Format(format!("safe XLSX patching does not yet support creating a new cell at {}!{}", sheet.name, a1)));
            };

            let original_cell = &text[start..end];
            let original_start = &text[start..start_tag_end];
            let old_type = attr_value(original_start, "t");
            let old_shared = old_type.as_deref() == Some("s");

            let value = new_cell.map(|cell| cell.value.scalar()).unwrap_or(Value::Empty);

            let (replacement, new_shared) = write_constant_cell(original_cell, &value, &mut sst)?;

            if old_shared != new_shared {
                sst.count_changed = true;
            }

            edits.push(Edit { start, end, replacement });
        }

        edits.sort_by_key(|a| std::cmp::Reverse(a.start));
        for edit in edits {
            text.replace_range(edit.start..edit.end, &edit.replacement);
        }

        replacements.insert(normalize(part), text.into_bytes());
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
