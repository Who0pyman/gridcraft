#!/usr/bin/env python3
"""ORGrid macro-enabled XLSX preservation smoke test.

Builds a synthetic OOXML workbook with shared formulas, an opaque VBA part,
and an external-link relationship. Never uploads or changes customer data.
Usage: python3 tools/orgrid_xlsm_smoke.py target/debug/gridcraft-cli
"""
import sys
import subprocess
import tempfile
import zipfile
from pathlib import Path
from xml.etree import ElementTree as ET

M = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"


def fixture(path):
    parts = {
        "[Content_Types].xml": """<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
<Override PartName="/xl/externalLinks/externalLink1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.externalLink+xml"/>
<Override PartName="/xl/vbaProject.bin" ContentType="application/vnd.ms-office.vbaProject"/>
</Types>""",
        "_rels/.rels": """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>""",
        "xl/workbook.xml": """<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
<externalReferences><externalReference r:id="rId3"/></externalReferences>
</workbook>""",
        "xl/_rels/workbook.xml.rels": """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
<Relationship Id="rId2" Type="http://schemas.microsoft.com/office/2006/relationships/vbaProject" Target="vbaProject.bin"/>
<Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLink" Target="externalLinks/externalLink1.xml"/>
</Relationships>""",
        "xl/worksheets/sheet1.xml": """<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>
<row r="1"><c r="A1"><v>2</v></c><c r="C1"><f t="shared" si="3" ref="C1:C2">A1*2</f><v>4</v></c><c r="E1"><f>[1]Sheet1!A1</f><v>7</v></c></row>
<row r="2"><c r="A2"><v>3</v></c><c r="C2"><f t="shared" si="3"/><v>6</v></c></row>
</sheetData>
<mergeCells count="1"><mergeCell ref="F1:G1"/></mergeCells>
<pageSetup orientation="landscape"/>
</worksheet>""",
        "xl/externalLinks/externalLink1.xml": """<?xml version="1.0" encoding="UTF-8"?>
<externalLink xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><externalBook r:id="rId1"/></externalLink>""",
        "xl/externalLinks/_rels/externalLink1.xml.rels": """<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/externalLinkPath" Target="https://example.invalid/linked.xlsx" TargetMode="External"/>
</Relationships>""",
        "xl/vbaProject.bin": b"SYNTHETIC-OPAQUE-VBA-PROJECT-CONTENT",
        "xl/media/opaque.bin": b"SYNTHETIC-OPAQUE-MEDIA-CONTENT",
    }
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in parts.items():
            z.writestr(name, data.encode() if isinstance(data, str) else data)


def parts(path):
    with zipfile.ZipFile(path) as z:
        bad = z.testzip()
        if bad is not None:
            raise AssertionError(f"Bad ZIP CRC: {bad}")
        return {name: z.read(name) for name in z.namelist()}


def sheet(data):
    return ET.fromstring(data)


def formula_map(root):
    out = {}
    for cell in root.iter(M + "c"):
        f = cell.find(M + "f")
        if f is None:
            continue
        v = cell.find(M + "v")
        out[cell.get("r")] = (f.text or "", dict(f.attrib), None if v is None else v.text)
    return out


def run(cli, original, output, address, value):
    command = f'cell.set={{"cell":"{address}","input":"{value}"}}'
    proc = subprocess.run(
        [str(cli), "run", "--in", str(original), "--cmd", command, "--out", str(output), "--quiet"],
        capture_output=True, text=True, check=False, timeout=180
    )
    if proc.returncode:
        raise AssertionError(f"GridCraft CLI failed: {proc.stderr}\n{proc.stdout}")
    if not output.is_file():
        raise AssertionError("CLI returned success but did not create the XLSM output")


def verify_ordinary_edit(before, after):
    a, b = parts(before), parts(after)
    assert set(a) == set(b), "Macro/external/package parts were added or lost"
    changes = {n for n in a if a[n] != b[n]}
    assert changes == {"xl/worksheets/sheet1.xml"}, f"Unexpected parts changed: {changes}"
    ar = sheet(a["xl/worksheets/sheet1.xml"])
    br = sheet(b["xl/worksheets/sheet1.xml"])
    assert formula_map(ar) == formula_map(br), "Existing formulas or caches changed"
    assert br.find(f".//{M}c[@r='D1']/{M}is/{M}t").text == "ORGRID QA", "Edited cell missing"
    assert ET.tostring(ar.find(M + "mergeCells")) == ET.tostring(br.find(M + "mergeCells")), "Merge layout changed"
    assert ET.tostring(ar.find(M + "pageSetup")) == ET.tostring(br.find(M + "pageSetup")), "Print settings changed"


def verify_shared_master_edit(before, after):
    a, b = parts(before), parts(after)
    assert set(a) == set(b), "Opaque workbook parts were lost"
    assert all(a[n] == b[n] for n in a if n != "xl/worksheets/sheet1.xml"), "Unrelated parts changed"
    br = sheet(b["xl/worksheets/sheet1.xml"])
    c1 = br.find(f".//{M}c[@r='C1']")
    c2 = br.find(f".//{M}c[@r='C2']")
    assert c1 is not None and c1.find(M + "v").text == "19", "Master edit missing"
    assert c1.find(M + "f") is None, "Master formula was not replaced"
    assert c2.find(M + "f").text.upper() == "A2*2", "Shared child formula was damaged"
    assert c2.find(M + "f").get("t") is None, "Shared child still requires removed master"
    assert c2.find(M + "v").text == "6", "Child's original cache was changed"


def main():
    if len(sys.argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    cli = Path(sys.argv[1]).resolve()
    if not cli.is_file():
        print(f"Missing compiled CLI: {cli}", file=sys.stderr)
        return 2
    with tempfile.TemporaryDirectory(prefix="orgrid-xlsm-qa-") as tmp:
        root = Path(tmp)
        source = root / "source.xlsm"
        basic = root / "edited.xlsm"
        master = root / "shared-master-edited.xlsm"
        fixture(source)
        run(cli, source, basic, "D1", "ORGRID QA")
        verify_ordinary_edit(source, basic)
        run(cli, source, master, "C1", "19")
        verify_shared_master_edit(source, master)
    print("PASS: synthetic XLSM ordinary cell edit and shared-master edit")
    print("PASS: VBA, external link, opaque parts, cached formulas, merge, print settings preserved")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
