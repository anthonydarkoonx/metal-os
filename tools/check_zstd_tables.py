#!/usr/bin/env python3
"""Check that the constant tables in libs/metal-zstd/src/tables.rs equal the ones printed in
RFC 8878 (sections 3.1.1.3.2.1 and 3.1.1.3.2.2). Usage: check_zstd_tables.py rfc8878.txt
(fetch it from https://www.rfc-editor.org/rfc/rfc8878.txt). The RFC is the sole source of
these constants; this script is the proof."""
import re
import sys

rfc = open(sys.argv[1]).read()
src = open(__file__.rsplit("/", 2)[0] + "/libs/metal-zstd/src/tables.rs").read()

def rust_array(name):
    m = re.search(r"const %s: \[\w+; \d+\] = \[(.*?)\];" % name, src, re.S)
    return [int(x) for x in re.findall(r"-?\d+", m.group(1))]

def rfc_code_table(title_re, count):
    """Rows '| code | baseline | bits |' between the table's header and its caption."""
    start = re.search(title_re, rfc).start()
    end = rfc.index("Table", start)
    base, bits = {}, {}
    for line in rfc[start:end].splitlines():
        m = re.match(r"\s*\|\s*(\d+)(?:-(\d+))?\s*\|\s*([\w +]+?)\s*\|\s*(\d+)\s*\|", line)
        if not m:
            continue
        lo, hi = int(m.group(1)), int(m.group(2) or m.group(1))
        for c in range(lo, hi + 1):
            b = m.group(3)
            base[c] = c + 3 if "Match_Length_Code + 3" in b else (c if b == "length" else int(b))
            bits[c] = int(m.group(4))
    assert sorted(base) == list(range(count)), (title_re, sorted(base))
    return [base[i] for i in range(count)], [bits[i] for i in range(count)]

def rfc_distribution(name):
    m = re.search(r"short %s\[\d+\] =\s*\{(.*?)\};" % name, rfc, re.S)
    return [int(x) for x in re.findall(r"-?\d+", m.group(1))]

ok = True
def check(label, mine, theirs):
    global ok
    same = mine == theirs
    ok &= same
    print(f"{'ok  ' if same else 'DIFF'} {label} ({len(mine)} entries)")
    if not same:
        print("   mine  ", mine)
        print("   rfc   ", theirs)

llb, llbits = rfc_code_table(r"\| Literals_Length_Code \| Baseline", 36)
mlb, mlbits = rfc_code_table(r"\| Match_Length_Code \|\s+Baseline", 53)
check("LL_BASE", rust_array("LL_BASE"), llb)
check("LL_BITS", rust_array("LL_BITS"), llbits)
check("ML_BASE", rust_array("ML_BASE"), mlb)
check("ML_BITS", rust_array("ML_BITS"), mlbits)
check("LL_DEFAULT", rust_array("LL_DEFAULT"), rfc_distribution("literalsLength_defaultDistribution"))
check("ML_DEFAULT", rust_array("ML_DEFAULT"), rfc_distribution("matchLengths_defaultDistribution"))
check("OF_DEFAULT", rust_array("OF_DEFAULT"), rfc_distribution("offsetCodes_defaultDistribution"))
sys.exit(0 if ok else 1)
