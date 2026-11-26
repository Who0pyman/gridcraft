//! File menu: new, open, save, close, export, properties.

use serde_json::{Value as Json, json};

use super::*;
use crate::DocState;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(noundo "file.new", "New Workbook", ["File"], Some("Cmd+N"), "{sample?: \"budget\"|\"sales\"|\"grades\"}", always, new_workbook),
        cmd!(noundo "file.open", "Open…", ["File"], Some("Cmd+O"), "{path} | {name, base64} (xlsx, xlsm, xlsb and ods data import, csv, tsv, txt, json)", always, open),
        cmd!(noundo "file.save", "Save", ["File"], Some("Cmd+S"), "{path?} (xlsx by default; .csv/.tsv/.json/.html by extension)", has_doc, save),
        cmd!(noundo "file.saveAs", "Save As…", ["File"], Some("Cmd+Shift+S"), "{path}", has_doc, save_as),
        cmd!(query "file.saveBytes", "Encode Workbook", [], None, "{format?: xlsx|csv|tsv|json|html} → {base64}", has_doc, save_bytes),
        cmd!(noundo "file.close", "Close", ["File"], Some("Cmd+W"), "{force?: bool}", has_doc, close),
        cmd!(noundo "file.exportCsv", "Export as CSV", ["File", "Export"], None, "{path, sheet?}", has_doc, |s, p| export(s, p, "csv")),
        cmd!(noundo "file.exportHtml", "Save as Web Page", ["File", "Export"], None, "{path}", has_doc, |s, p| export(s, p, "html")),
        cmd!("file.properties", "Properties", ["File"], None, "{title?, subject?, author?, company?, keywords?, description?}", has_doc, properties),
        cmd!(noundo "window.activate", "Switch Window", ["Window"], None, "{index}", has_doc, activate_window),
    ]
}

fn new_workbook(s: &mut Session, p: &Json) -> Result<Json> {
    let i = match str_param(p, "sample") {
        Some(name) => {
            let wb = crate::sample::build(name).ok_or_else(|| bad("file.new", format!("unknown sample `{name}`")))?;
            let title = crate::sample::title(name);
            s.add_document(DocState::new(wb, None, title))
        }
        None => s.new_workbook(),
    };
    Ok(json!({"index": i, "title": s.doc()?.display_title()}))
}

fn open(s: &mut Session, p: &Json) -> Result<Json> {
    let (name, bytes) = if let Some(b) = str_param(p, "base64") {
        (str_param(p, "name").unwrap_or("Book.xlsx").to_string(), crate::io::base64_decode(b).ok_or_else(|| bad("file.open", "invalid base64"))?)
    } else if let Some(path) = str_param(p, "path") {
        // Already open? Switch to it.
        if let Some(i) = s.documents().iter().position(|d| d.path.as_deref() == Some(path)) {
            s.set_active(i);
            return Ok(json!({"index": i, "alreadyOpen": true}));
        }
        (path.to_string(), crate::io::read_file(path)?)
    } else {
        s.ui_requests.push(crate::UiRequest::Dialog("open".into(), json!({})));
        return ok();
    };
    let (wb, warnings) = crate::io::open_bytes(&name, &bytes)?;
    let imported = matches!(crate::io::FileKind::from_path(&name), Some(crate::io::FileKind::Ods | crate::io::FileKind::Xlsb))
        || matches!(gridcraft_xlsx::sniff(&bytes), gridcraft_xlsx::Format::Ods | gridcraft_xlsx::Format::Xlsb);
    let title = if imported {
        format!("{} (imported).xlsx", std::path::Path::new(&name).file_stem().and_then(|n| n.to_str()).unwrap_or("Book"))
    } else {
        std::path::Path::new(&name).file_name().and_then(|n| n.to_str()).unwrap_or("Book").to_string()
    };
    // An import has no save target: ordinary Save must ask for a new destination.
    let path = str_param(p, "path").filter(|_| !imported).map(str::to_string);
    // Replace an untouched blank Book1 like Excel does.
    if s.documents().len() == 1
        && s.active().is_some_and(|d| d.path.is_none() && !d.is_dirty() && d.undo.is_empty() && d.wb.sheets.iter().all(|sh| sh.cells.is_empty()))
    {
        s.close_document(0);
    }
    // Imported Excel workbooks already contain Excel-calculated caches.
    // Don't replace those values with GridCraft's results just by opening the file.
    let is_excel = !imported && matches!(crate::io::FileKind::from_path(&name), Some(crate::io::FileKind::Xlsx));
    let mut doc = if is_excel {
        DocState::from_excel(wb, path, title)
    } else {
        DocState::new(wb, path, title)
    };

    // ORGrid: retain the original XLSX package so an untouched workbook
    // can be saved without destroying unsupported Excel package parts.
    if !imported && matches!(crate::io::FileKind::from_path(&name), Some(crate::io::FileKind::Xlsx)) {
        doc.original_xlsx = Some(bytes.clone());
    }

    let i = s.add_document(doc);
    if !warnings.is_empty() {
        let mut message = warnings.iter().take(5).cloned().collect::<Vec<_>>().join("\n\n");
        if warnings.len() > 5 {
            message.push_str(&format!("\n\n{} additional import warnings.", warnings.len() - 5));
        }
        s.ui_requests.push(crate::UiRequest::Message(message));
    }
    let d = s.doc()?;
    Ok(
        json!({"index": i, "title": d.display_title(), "sheets": d.wb.sheets.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), "warnings": warnings}),
    )
}

/// Macro-enabled Excel packages must never pass through GridCraft's full XLSX writer:
/// that writer does not serialize VBA projects or external workbook relationships.
fn macro_enabled_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xlsm") || ext.eq_ignore_ascii_case("xltm"))
}

fn macro_preservation_error(reason: &str) -> EngineError {
    EngineError::Other(format!(
        "The macro-enabled workbook was NOT saved. {reason} GridCraft cannot safely rewrite the VBA and external-link parts of this file. Use Excel for this operation; your original file is unchanged."
    ))
}

fn xlsx_preservation_error(reason: &str) -> EngineError {
    EngineError::Other(format!(
        "The imported Excel workbook was NOT saved. {reason} GridCraft cannot safely rebuild every original workbook component. The original file is unchanged."
    ))
}

/// Package patching writes cell content and styles, not every workbook feature.
/// If the in-memory model has other edits, returning success would silently lose
/// those edits while claiming the file was saved. Fail rather than discard them.
pub(crate) fn unsupported_cell_patch_change(before: &gridcraft_model::Workbook, after: &gridcraft_model::Workbook) -> Option<String> {
    if before.sheets.len() != after.sheets.len() {
        return Some("worksheet count changed".into());
    }
    if before.names != after.names {
        return Some("defined names changed".into());
    }
    if before.theme != after.theme || before.date_system != after.date_system || before.calc != after.calc {
        return Some("workbook theme or calculation settings changed".into());
    }
    if before.props != after.props
        || before.protected_structure != after.protected_structure
        || before.custom_lists != after.custom_lists
        || before.cell_styles != after.cell_styles
    {
        return Some("workbook properties or named styles changed".into());
    }
    for (previous, current) in before.sheets.iter().zip(&after.sheets) {
        if previous.name != current.name
            // Safe XLSX patching now writes row heights, hidden rows and outlines.
            || previous.cols != current.cols
            || previous.default_col_width != current.default_col_width
            || previous.default_row_height != current.default_row_height
            // The OOXML save path now patches mergeCells independently.
            || previous.freeze != current.freeze
            || previous.tab_color != current.tab_color
            || previous.visibility != current.visibility
            || previous.show_gridlines != current.show_gridlines
            || previous.show_headings != current.show_headings
            || previous.show_formulas != current.show_formulas
            || previous.show_zeros != current.show_zeros
            || previous.right_to_left != current.right_to_left
            || previous.zoom != current.zoom
            || previous.cond_formats != current.cond_formats
            || previous.validations != current.validations
            || previous.tables != current.tables
            || previous.autofilter != current.autofilter
            || previous.cell_pictures != current.cell_pictures
            || previous.comments != current.comments
            || previous.hyperlinks != current.hyperlinks
            || previous.charts != current.charts
            || previous.images != current.images
            || previous.shapes != current.shapes
            || previous.sparklines != current.sparklines
            || previous.pivots != current.pivots
            || previous.protection != current.protection
            || previous.print != current.print
        {
            return Some(format!("unpatchable worksheet features changed on {}", previous.name));
        }
    }
    None
}

fn save(s: &mut Session, p: &Json) -> Result<Json> {
    let path = match str_param(p, "path").map(str::to_string).or_else(|| s.active().and_then(|d| d.path.clone())) {
        Some(p) => p,
        None => {
            s.ui_requests.push(crate::UiRequest::Dialog("saveAs".into(), json!({})));
            return ok();
        }
    };
    let d = s.doc_mut()?;
    let kind = crate::io::FileKind::from_path(&path).unwrap_or(crate::io::FileKind::Xlsx);
    let source_name = d.path.as_deref().unwrap_or(&d.title);
    let macro_workbook = d.original_xlsx.is_some() && macro_enabled_path(source_name);

    if matches!(kind, crate::io::FileKind::Xlsx) {
        if d.original_xlsx.is_some() && macro_enabled_path(source_name) != macro_enabled_path(&path) {
            return Err(macro_preservation_error(
                "Changing between macro-enabled and macro-free workbook formats would invalidate the original Excel package.",
            ));
        }
        if d.original_xlsx.is_none() && macro_enabled_path(&path) {
            return Err(macro_preservation_error(
                "Creating a new macro-enabled workbook without an original Excel package is unsupported.",
            ));
        }
    }

    let bytes = if matches!(kind, crate::io::FileKind::Xlsx) {
        match &d.original_xlsx {
            Some(original) if !d.is_dirty() => original.clone(),
            Some(original) if !d.xlsx_structural_dirty && d.xlsx_structural_edits.is_empty() => {
                if let Some(change) = unsupported_cell_patch_change(&d.saved, &d.wb) {
                    return Err(xlsx_preservation_error(&format!(
                        "This edit cannot be represented by a cell-only package patch ({change})."
                    )));
                }
                gridcraft_xlsx::patch_xlsx_with_style_copies(original, &d.saved, &d.wb, &d.xlsx_dirty_cells, &d.xlsx_style_copies)
                    .map_err(|e| EngineError::Other(format!("Safe XLSX patch save failed: {e}")))?
            }
            Some(original) if !d.xlsx_structural_dirty => {
                if !d.xlsx_style_copies.is_empty() {
                    return Err(xlsx_preservation_error(
                        "Combining structural row/column edits with exact Excel format-copy hints is not yet package-safe.",
                    ));
                }
                match gridcraft_xlsx::patch_xlsx_structural(original, &d.saved, &d.wb, &d.xlsx_dirty_cells, &d.xlsx_structural_edits) {
                    Ok(bytes) => bytes,
                    Err(error) if macro_workbook => {
                        return Err(macro_preservation_error(&format!(
                            "The structural XLSX patch could not preserve this edit ({error})."
                        )));
                    }
                    Err(error) => {
                        return Err(xlsx_preservation_error(&format!(
                            "The structural package-preserving patch failed ({error})."
                        )));
                    }
                }
            }
            Some(_) if macro_workbook => {
                return Err(macro_preservation_error(
                    "This edit requires a complete workbook rewrite, which would discard unsupported Excel components.",
                ));
            }
            Some(_) => {
                return Err(xlsx_preservation_error(
                    "This edit requires a complete workbook rewrite.",
                ));
            }
            _ => crate::io::save_bytes(&d.wb, &path)?,
        }
    } else {
        crate::io::save_bytes(&d.wb, &path)?
    };

    crate::io::write_file(&path, &bytes)?;

    // Only a full-fidelity format clears the dirty flag and becomes the document's file.
    if matches!(kind, crate::io::FileKind::Xlsx | crate::io::FileKind::Json) {
        d.saved = d.wb.clone();
        d.path = Some(path.clone());

        if matches!(kind, crate::io::FileKind::Xlsx) {
            d.original_xlsx = Some(bytes.clone());
            d.xlsx_dirty_cells.clear();
            d.xlsx_style_copies.clear();
            d.xlsx_structural_edits.clear();
            d.xlsx_structural_dirty = false;
            d.xlsx_epoch = d.xlsx_epoch.wrapping_add(1);
        } else {
            // A JSON save no longer has an XLSX package that corresponds to `saved`.
            d.original_xlsx = None;
            d.xlsx_dirty_cells.clear();
            d.xlsx_style_copies.clear();
            d.xlsx_structural_edits.clear();
            d.xlsx_structural_dirty = false;
        }
    }
    Ok(json!({"path": path, "bytes": bytes.len()}))
}

fn save_as(s: &mut Session, p: &Json) -> Result<Json> {
    if str_param(p, "path").is_none() {
        s.ui_requests.push(crate::UiRequest::Dialog("saveAs".into(), json!({})));
        return ok();
    }
    save(s, p)
}

fn save_bytes(s: &mut Session, p: &Json) -> Result<Json> {
    let fmt = str_param(p, "format").unwrap_or("xlsx");
    let d = s.doc()?;
    let out = format!("x.{fmt}");
    let kind = crate::io::FileKind::from_path(&out).unwrap_or(crate::io::FileKind::Xlsx);
    let bytes = if matches!(kind, crate::io::FileKind::Xlsx) {
        let source_name = d.path.as_deref().unwrap_or(&d.title);
        if d.original_xlsx.is_some() && macro_enabled_path(source_name) != macro_enabled_path(&out) {
            return Err(macro_preservation_error(
                "Changing between macro-enabled and macro-free workbook formats would invalidate the original Excel package.",
            ));
        }
        match &d.original_xlsx {
            Some(original) if !d.is_dirty() => original.clone(),
            Some(original) if !d.xlsx_structural_dirty && d.xlsx_structural_edits.is_empty() => {
                if let Some(change) = unsupported_cell_patch_change(&d.saved, &d.wb) {
                    return Err(xlsx_preservation_error(&format!(
                        "This edit cannot be represented by a cell-only package patch ({change})."
                    )));
                }
                gridcraft_xlsx::patch_xlsx_with_style_copies(original, &d.saved, &d.wb, &d.xlsx_dirty_cells, &d.xlsx_style_copies)
                    .map_err(|e| xlsx_preservation_error(&format!("Safe XLSX patch failed ({e}).")))?
            }
            Some(original) if !d.xlsx_structural_dirty => {
                if !d.xlsx_style_copies.is_empty() {
                    return Err(xlsx_preservation_error(
                        "Combining structural edits with exact Excel format-copy hints is not yet package-safe.",
                    ));
                }
                gridcraft_xlsx::patch_xlsx_structural(original, &d.saved, &d.wb, &d.xlsx_dirty_cells, &d.xlsx_structural_edits)
                    .map_err(|e| xlsx_preservation_error(&format!("Safe structural XLSX patch failed ({e}).")))?
            }
            Some(_) => {
                return Err(xlsx_preservation_error(
                    "This edit requires an unsafe full workbook rewrite.",
                ));
            }
            None if macro_enabled_path(&out) => {
                return Err(macro_preservation_error(
                    "Creating a macro-enabled workbook without an original Excel package is unsupported.",
                ));
            }
            None => crate::io::save_bytes(&d.wb, &out)?,
        }
    } else {
        crate::io::save_bytes(&d.wb, &out)?
    };
    let name = format!("{}.{fmt}", std::path::Path::new(&d.display_title()).file_stem().and_then(|x| x.to_str()).unwrap_or("Book"));
    Ok(json!({"name": name, "base64": crate::io::base64_encode(&bytes), "bytes": bytes.len()}))
}

fn close(s: &mut Session, p: &Json) -> Result<Json> {
    let force = bool_param(p, "force").unwrap_or(false);
    let d = s.doc()?;
    if d.is_dirty() && !force {
        s.ui_requests.push(crate::UiRequest::Dialog("saveChanges".into(), json!({"title": d.display_title()})));
        return Ok(json!({"closed": false, "dirty": true}));
    }
    let i = s.active_index();
    s.close_document(i);
    Ok(json!({"closed": true}))
}

fn export(s: &mut Session, p: &Json, ext: &str) -> Result<Json> {
    let path = str_param(p, "path").ok_or_else(|| bad("file.export", "missing `path`"))?.to_string();
    let d = s.doc()?;
    let mut wb = (*d.wb).clone();
    if p.get("sheet").is_some() {
        wb.active_sheet = target_sheet(s, p)?;
    }
    let target = if path.ends_with(&format!(".{ext}")) { path.clone() } else { format!("{path}.{ext}") };
    let bytes = crate::io::save_bytes(&wb, &target)?;
    crate::io::write_file(&target, &bytes)?;
    Ok(json!({"path": target, "bytes": bytes.len()}))
}

fn properties(s: &mut Session, p: &Json) -> Result<Json> {
    edit(s, |cx| {
        let pr = &mut cx.wb.props;
        for (k, slot) in [
            ("title", &mut pr.title),
            ("subject", &mut pr.subject),
            ("author", &mut pr.author),
            ("company", &mut pr.company),
            ("keywords", &mut pr.keywords),
            ("description", &mut pr.description),
        ] {
            if let Some(v) = str_param(p, k) {
                *slot = v.to_string();
            }
        }
        Ok(serde_json::to_value(&cx.wb.props).unwrap_or_default())
    })
}

fn activate_window(s: &mut Session, p: &Json) -> Result<Json> {
    let i = u32_param(p, "index").ok_or_else(|| bad("window.activate", "missing `index`"))? as usize;
    if i >= s.documents().len() {
        return Err(bad("window.activate", "no such window"));
    }
    s.set_active(i);
    ok()
}
