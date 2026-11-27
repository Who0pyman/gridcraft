#!/usr/bin/env python3
"""Test a local GridCraft build against a private complex .xlsm workbook.

Never modifies the source file or uploads it to a network service.

Usage:
  python3 tools/orgrid_real_workbook_qa.py /path/to/gridcraft-cli "/path/to/workbook.xlsm"

This audit checks ordinary edit, exact-style copy, merged areas, whole-row copy, and undo-after-save.
It compares every decompressed OOXML component, including opaque VBA parts.
"""
import argparse
import json
import posixpath
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

S = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
R = "{http://schemas.openxmlformats.org/officeDocument/2006/relationships}"
P = "{http://schemas.openxmlformats.org/package/2006/relationships}"


def package(path):
    with zipfile.ZipFile(path) as z:
        broken = z.testzip()
        if broken:
            raise AssertionError(f"Bad ZIP CRC: {broken}")
        return {info.filename: z.read(info.filename)
                for info in z.infolist() if not info.is_dir()}


def sheet_part(parts, sheet_name):
    root = ET.fromstring(parts["xl/workbook.xml"])
    item = next((x for x in root.findall(".//" + S + "sheet") if x.get("name") == sheet_name), None)
    if item is None:
        raise AssertionError(f"Missing sheet {sheet_name}")
    rid = item.get(R + "id")
    rels = ET.fromstring(parts["xl/_rels/workbook.xml.rels"])
    rel = next((x for x in rels.findall(P + "Relationship") if x.get("Id") == rid), None)
    if rel is None:
        raise AssertionError(f"No package relationship for sheet {sheet_name}")
    target = rel.attrib["Target"].lstrip("/")
    return target if target.startswith("xl/") else posixpath.normpath("xl/" + target)


def cell(parts, sheet, address):
    part = sheet_part(parts, sheet)
    root = ET.fromstring(parts[part])
    return root.find(f".//{S}c[@r='{address}']")


def run(cli, input_path, output_path, commands):
    args = [str(cli), "run", "--in", str(input_path)]
    for name, data in commands:
        args.extend(["--cmd", f"{name}={json.dumps(data, separators=(',', ':'))}"])
    args.extend(["--out", str(output_path), "--quiet"])
    proc = subprocess.run(args, text=True, capture_output=True, timeout=240)
    if proc.returncode:
        raise AssertionError(f"GridCraft returned {proc.returncode}:\n{proc.stderr[-3000:]}")
    if not output_path.exists():
        raise AssertionError("Output workbook missing")


def compare_components(before, after, allowed, formula_graph_changed=False, allow_no_changes=False):
    added = after.keys() - before.keys()
    missing = before.keys() - after.keys()
    permitted_changes = set(allowed)
    chain_path = "xl/calcChain.xml"
    expected_missing = {chain_path} if formula_graph_changed and chain_path in before else set()
    if added or missing != expected_missing:
        raise AssertionError(f"Components added={sorted(added)}, removed={sorted(missing)}, expected removed={sorted(expected_missing)}")
    if expected_missing:
        metadata = {"[Content_Types].xml", "xl/_rels/workbook.xml.rels"}
        for part in metadata:
            assert b"calcChain" not in after[part], f"Stale calculation chain reference in {part}"
        permitted_changes.update(metadata)
    changed = {k for k in before.keys() & after.keys() if before[k] != after[k]}
    excess = changed - permitted_changes
    if excess:
        raise AssertionError(f"Unexpected changed components: {sorted(excess)}")
    if not changed and not allow_no_changes:
        raise AssertionError("Edit did not modify any workbook component")
    for key in before:
        if key.endswith("vbaProject.bin") or "/externalLinks/" in key:
            if before[key] != after[key]:
                raise AssertionError(f"Private Excel component damaged: {key}")
    return changed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cli", type=Path, help="Compiled gridcraft-cli executable")
    parser.add_argument("workbook", type=Path, help="Private .xlsm workbook")
    parser.add_argument("--sheet", default="MI Overview")
    parser.add_argument("--copy-sheet", default="LEASEHOLD-DS1")
    parser.add_argument("--source-cell", default="AM18")
    parser.add_argument("--destination-cell", default="AM900")
    args = parser.parse_args()

    cli, workbook = args.cli.resolve(), args.workbook.resolve()
    if not cli.is_file() or not workbook.is_file():
        parser.error("Both compiled CLI and private workbook must exist")
    if workbook.suffix.lower() != ".xlsm":
        parser.error("Use a macro-enabled XLSM copy for this audit")

    original = package(workbook)
    assert any(k.endswith("vbaProject.bin") for k in original), "Fixture is missing VBA project"
    change_part = sheet_part(original, args.sheet)
    copy_part = sheet_part(original, args.copy_sheet)
    check_cell = "ZZ1000"
    assert cell(original, args.sheet, check_cell) is None, f"{check_cell} must be an empty QA cell"
    assert cell(original, args.copy_sheet, args.destination_cell) is None, "Copy destination must be empty"

    with tempfile.TemporaryDirectory(prefix="orgrid-local-qa-") as directory:
        work = Path(directory)

        basic_file = work / "ordinary.xlsm"
        run(cli, workbook, basic_file, [
            ("sheet.activate", {"sheet": args.sheet}),
            ("cell.set", {"cell": check_cell, "input": "ORGRID QA"}),
        ])
        basic = package(basic_file)
        changes = compare_components(
            original, basic, {change_part, "xl/sharedStrings.xml", "xl/styles.xml"}
        )
        assert cell(basic, args.sheet, check_cell) is not None, "Ordinary edit missing"
        print("PASS ordinary edit: preserved all unrelated workbook components", sorted(changes))

        copy_file = work / "copy.xlsm"
        run(cli, workbook, copy_file, [
            ("sheet.activate", {"sheet": args.copy_sheet}),
            ("edit.copy", {"range": args.source_cell}),
            ("edit.paste", {"at": args.destination_cell}),
        ])
        copied = package(copy_file)
        changes = compare_components(original, copied, {copy_part, "xl/styles.xml"}, formula_graph_changed=True)
        source = cell(original, args.copy_sheet, args.source_cell)
        dest = cell(copied, args.copy_sheet, args.destination_cell)
        assert source is not None and dest is not None, "Copied source or destination missing"
        assert source.get("s", "0") == dest.get("s", "0"), (
            f"Excel style mismatch: source XF {source.get('s', '0')}, "
            f"destination XF {dest.get('s', '0')}"
        )
        source_formula, dest_formula = source.find(S + "f"), dest.find(S + "f")
        assert source_formula is not None and dest_formula is not None, "Copied formula missing"
        print("PASS copy: same exact XF style, formula copied, package intact", sorted(changes))

        # Validate copied merges, including the original opaque workbook parts.
        merged_file = work / "merged-copy.xlsm"
        run(cli, workbook, merged_file, [
            ("sheet.activate", {"sheet": args.copy_sheet}),
            ("edit.copy", {"range": "AJ18:AK18"}),
            ("edit.paste", {"at": "AJ900"}),
        ])
        merged = package(merged_file)
        merged_has_formula = any(
            node is not None and node.find(S + "f") is not None
            for node in (cell(merged, args.copy_sheet, address) for address in ("AJ900", "AK900"))
        )
        compare_components(original, merged, {copy_part, "xl/styles.xml"}, formula_graph_changed=merged_has_formula)
        merged_sheet = ET.fromstring(merged[copy_part])
        merge_refs = {node.get("ref") for node in merged_sheet.iter(S + "mergeCell")}
        assert "AJ900:AK900" in merge_refs, "Copied Excel merged cells were not saved"
        print("PASS merged-range copy: new merge persisted, opaque package intact")

        # A saved merged copy must undo cleanly: no styled blank cells and
        # no synthetic row left behind in the original worksheet.
        merge_saved = work / "merge-saved.xlsm"
        merge_undone = work / "merge-undone.xlsm"
        run(cli, workbook, merge_undone, [
            ("sheet.activate", {"sheet": args.copy_sheet}),
            ("edit.copy", {"range": "AJ18:AK18"}),
            ("edit.paste", {"at": "AJ900"}),
            ("file.save", {"path": str(merge_saved)}),
            ("edit.undo", {}),
            ("file.save", {"path": str(merge_undone)}),
        ])
        restored = package(merge_undone)
        compare_components(
            original, restored, {copy_part, "xl/styles.xml"},
            formula_graph_changed=merged_has_formula, allow_no_changes=True,
        )
        assert restored[copy_part] == original[copy_part], (
            "Undo left behind copied cell formatting, row records, or merge XML"
        )
        print("PASS saved merged-range undo: original worksheet restored exactly")


        # Copy the actual leasehold row. Check exact Excel style indices, not just
        # normalized style equivalence, for every copied populated source cell.
        row_file = work / "row-copy.xlsm"
        run(cli, workbook, row_file, [
            ("sheet.activate", {"sheet": args.copy_sheet}),
            ("edit.copy", {"range": "A18:KQ18"}),
            ("edit.paste", {"at": "A900"}),
        ])
        row = package(row_file)
        compare_components(original, row, {copy_part, "xl/styles.xml", "xl/sharedStrings.xml"}, formula_graph_changed=True)
        original_row = ET.fromstring(original[copy_part])
        copied_row = ET.fromstring(row[copy_part])
        dest_by_ref = {c.get("r"): c for c in copied_row.iter(S + "c")}
        cells_checked = formulas_checked = 0
        for src_cell in original_row.iter(S + "c"):
            address = src_cell.get("r", "")
            if not re.fullmatch(r"[A-Z]+18", address):
                continue
            target = re.match(r"[A-Z]+", address).group() + "900"
            copy_cell = dest_by_ref.get(target)
            assert copy_cell is not None, f"Row copy lost {address}"
            assert src_cell.get("s", "0") == copy_cell.get("s", "0"), (
                f"Exact Excel style mismatch for {address} -> {target}"
            )
            cells_checked += 1
            if src_cell.find(S + "f") is not None:
                assert copy_cell.find(S + "f") is not None, f"Copied formula missing at {target}"
                formulas_checked += 1
        assert cells_checked > 0 and formulas_checked > 0
        assert "AJ900:AK900" in {node.get("ref") for node in copied_row.iter(S + "mergeCell")}
        print(f"PASS row copy: {cells_checked} cell styles, {formulas_checked} formulas, merges, and package intact")

        first = work / "undo-first.xlsm"
        final = work / "undo-final.xlsm"
        run(cli, workbook, final, [
            ("sheet.activate", {"sheet": args.sheet}),
            ("cell.set", {"cell": check_cell, "input": "ORGRID MUST BE UNDONE"}),
            ("file.save", {"path": str(first)}),
            ("edit.undo", {}),
            ("file.save", {"path": str(final)}),
        ])
        undone = package(final)
        if cell(undone, args.sheet, check_cell) is not None:
            raise AssertionError("CRITICAL: Undo after save did not remove the edit")
        assert undone.keys() == original.keys(), "Undo after save lost workbook components"
        print("PASS save/undo/save: undone cell absent and all package parts retained")

        # A title calculation edit must persist dependent Excel cache values.
        # Without this check an XLSM looks right on screen, but reopens with
        # stale ownership calculations. No original template formulas change.
        recalc_file = work / "dependent-caches.xlsm"
        run(cli, workbook, recalc_file, [
            ("sheet.activate", {"sheet": args.copy_sheet}),
            ("cell.set", {"cell": "M18", "input": "0.3"}),
        ])
        recalc = package(recalc_file)
        allowed_calculation_parts = {part for part in original if part.startswith("xl/worksheets/")}
        allowed_calculation_parts.update({"xl/styles.xml", "xl/sharedStrings.xml"})
        compare_components(original, recalc, allowed_calculation_parts, formula_graph_changed=True)
        def numeric(parts, address):
            item = cell(parts, args.copy_sheet, address)
            value = item.find(S + "v") if item is not None else None
            assert value is not None and value.text is not None, f"No cached value for {address}"
            return float(value.text)
        assert abs(numeric(recalc, "AM18") - 0.3) < 1e-12, "AM18 cached WI formula is stale"
        assert abs(numeric(recalc, "AP18") - 0.313125) < 1e-12, "AP18 cached royalty formula is stale"
        assert abs(numeric(recalc, "AL18") - numeric(recalc, "AJ18") * 0.7) < 1e-10, "AL18 cached NRI formula is stale"
        for address in ("AM18", "AL18", "AP18"):
            old_f = cell(original, args.copy_sheet, address).find(S + "f")
            new_f = cell(recalc, args.copy_sheet, address).find(S + "f")
            assert old_f is not None and new_f is not None
            assert ET.tostring(old_f) == ET.tostring(new_f), f"Formula XML was altered at {address}"
        print("PASS updated WI/NRI caches: Excel formulas preserved, reopened numeric results current")


    print("PASS actual-workbook QA checks. This does not certify VBA execution or full Excel feature parity.")


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, subprocess.TimeoutExpired, zipfile.BadZipFile, ET.ParseError) as exc:
        print("FAIL", str(exc), file=sys.stderr)
        raise SystemExit(1)
