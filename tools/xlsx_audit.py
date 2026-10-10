import sys
import zipfile
import hashlib
import posixpath
import xml.etree.ElementTree as ET
from pathlib import Path

NS_MAIN = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
NS_REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
NS_PKGREL = "http://schemas.openxmlformats.org/package/2006/relationships"

def sha(data):
    return hashlib.sha256(data).hexdigest()

def read_xml(z, name):
    return ET.fromstring(z.read(name))

def workbook_sheets(z):
    wb = read_xml(z, "xl/workbook.xml")
    rels = read_xml(z, "xl/_rels/workbook.xml.rels")

    relmap = {}
    for rel in rels.findall(f"{{{NS_PKGREL}}}Relationship"):
        relmap[rel.attrib["Id"]] = rel.attrib["Target"]

    result = {}
    sheets = wb.find(f"{{{NS_MAIN}}}sheets")
    if sheets is None:
        return result

    for s in sheets:
        name = s.attrib["name"]
        rid = s.attrib.get(f"{{{NS_REL}}}id")
        target = relmap.get(rid, "")
        if target.startswith("/"):
            target = target.lstrip("/")
        elif not target.startswith("xl/"):
            target = posixpath.normpath(posixpath.join("xl", target))
        result[name] = target

    return result

def sheet_info(z, path):
    root = read_xml(z, path)

    formulas = {}
    cached = {}

    for c in root.findall(f".//{{{NS_MAIN}}}c"):
        ref = c.attrib.get("r")
        f = c.find(f"{{{NS_MAIN}}}f")
        v = c.find(f"{{{NS_MAIN}}}v")

        if f is not None:
            formulas[ref] = {
                "text": f.text or "",
                "attrs": dict(f.attrib),
            }
            cached[ref] = v.text if v is not None else None

    merges = set()
    for m in root.findall(f".//{{{NS_MAIN}}}mergeCell"):
        if "ref" in m.attrib:
            merges.add(m.attrib["ref"])

    hidden_rows = set()
    for r in root.findall(f".//{{{NS_MAIN}}}row"):
        if r.attrib.get("hidden") in ("1", "true", "True"):
            hidden_rows.add(r.attrib.get("r"))

    hidden_cols = set()
    for c in root.findall(f".//{{{NS_MAIN}}}col"):
        if c.attrib.get("hidden") in ("1", "true", "True"):
            hidden_cols.add((c.attrib.get("min"), c.attrib.get("max")))

    dim = root.find(f"{{{NS_MAIN}}}dimension")
    dimension = dim.attrib.get("ref") if dim is not None else None

    return {
        "formulas": formulas,
        "cached": cached,
        "merges": merges,
        "hidden_rows": hidden_rows,
        "hidden_cols": hidden_cols,
        "dimension": dimension,
    }

def main():
    if len(sys.argv) != 3:
        print("Usage: python3 tools/xlsx_audit.py ORIGINAL.xlsx OUTPUT.xlsx")
        sys.exit(2)

    original = Path(sys.argv[1])
    output = Path(sys.argv[2])

    report = []

    with zipfile.ZipFile(original) as za, zipfile.ZipFile(output) as zb:
        a_files = set(za.namelist())
        b_files = set(zb.namelist())

        only_a = sorted(a_files - b_files)
        only_b = sorted(b_files - a_files)

        changed = []
        for name in sorted(a_files & b_files):
            if sha(za.read(name)) != sha(zb.read(name)):
                changed.append(name)

        report.append("=== ORGRID XLSX AUDIT ===")
        report.append("")
        report.append(f"Original: {original}")
        report.append(f"Output:   {output}")
        report.append("")
        report.append(f"Package parts only in original: {len(only_a)}")
        for x in only_a:
            report.append(f"  - {x}")

        report.append("")
        report.append(f"Package parts only in output: {len(only_b)}")
        for x in only_b:
            report.append(f"  + {x}")

        report.append("")
        report.append(f"Changed common package parts: {len(changed)}")
        for x in changed:
            report.append(f"  * {x}")

        report.append("")
        report.append("=== WORKSHEET AUDIT ===")

        sheets_a = workbook_sheets(za)
        sheets_b = workbook_sheets(zb)

        all_sheet_names = sorted(set(sheets_a) | set(sheets_b))

        for sheet_name in all_sheet_names:
            report.append("")
            report.append(f"[{sheet_name}]")

            if sheet_name not in sheets_a:
                report.append("  Sheet exists only in output")
                continue

            if sheet_name not in sheets_b:
                report.append("  Sheet exists only in original")
                continue

            pa = sheets_a[sheet_name]
            pb = sheets_b[sheet_name]

            if pa not in a_files or pb not in b_files:
                report.append(f"  Could not locate worksheet XML: {pa} / {pb}")
                continue

            ia = sheet_info(za, pa)
            ib = sheet_info(zb, pb)

            report.append(
                f"  Formula cells: original={len(ia['formulas'])}, output={len(ib['formulas'])}"
            )

            formula_cells = sorted(set(ia["formulas"]) | set(ib["formulas"]))
            formula_diffs = []

            for cell in formula_cells:
                fa = ia["formulas"].get(cell)
                fb = ib["formulas"].get(cell)
                if fa != fb:
                    formula_diffs.append((cell, fa, fb))

            report.append(f"  Formula-definition differences: {len(formula_diffs)}")
            for cell, fa, fb in formula_diffs[:25]:
                report.append(f"    {cell}:")
                report.append(f"      original={fa}")
                report.append(f"      output  ={fb}")

            cached_diffs = []
            common_formula_cells = set(ia["formulas"]) & set(ib["formulas"])

            for cell in sorted(common_formula_cells):
                va = ia["cached"].get(cell)
                vb = ib["cached"].get(cell)
                if va != vb:
                    cached_diffs.append((cell, va, vb))

            report.append(f"  Cached formula-value differences: {len(cached_diffs)}")
            for cell, va, vb in cached_diffs[:25]:
                report.append(
                    f"    {cell}: original={va!r}, output={vb!r}"
                )

            added_merges = sorted(ib["merges"] - ia["merges"])
            removed_merges = sorted(ia["merges"] - ib["merges"])

            report.append(
                f"  Merges: original={len(ia['merges'])}, output={len(ib['merges'])}"
            )
            if removed_merges:
                report.append(f"    Removed merges: {removed_merges[:25]}")
            if added_merges:
                report.append(f"    Added merges: {added_merges[:25]}")

            if ia["dimension"] != ib["dimension"]:
                report.append(
                    f"  Dimension changed: {ia['dimension']} -> {ib['dimension']}"
                )

            if ia["hidden_rows"] != ib["hidden_rows"]:
                report.append(
                    f"  Hidden rows changed: {sorted(ia['hidden_rows'])} -> {sorted(ib['hidden_rows'])}"
                )

            if ia["hidden_cols"] != ib["hidden_cols"]:
                report.append(
                    f"  Hidden columns changed: {sorted(ia['hidden_cols'])} -> {sorted(ib['hidden_cols'])}"
                )

    out = Path("test-workbooks/xlsx-audit.txt")
    out.write_text("\n".join(report), encoding="utf-8")

    print(f"Audit complete.")
    print(f"Report written to: {out}")

if __name__ == "__main__":
    main()
