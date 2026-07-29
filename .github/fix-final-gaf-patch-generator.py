from pathlib import Path

path = Path('.github/apply-final-gaf-audit.py')
text = path.read_text()
old = "    file.write(r'''"
new = "    file.write('''"
if text.count(old) != 1:
    raise SystemExit(f'expected one raw Go test string, got {text.count(old)}')
path.write_text(text.replace(old, new, 1))
