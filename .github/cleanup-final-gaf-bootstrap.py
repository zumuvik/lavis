from pathlib import Path

path = Path('.github/workflows/ci.yml')
text = path.read_text()
start_marker = '      # BEGIN FINAL GAF AUDIT BOOTSTRAP\n'
end_marker = '      # END FINAL GAF AUDIT BOOTSTRAP\n'
start = text.index(start_marker)
end = text.index(end_marker, start) + len(end_marker)
text = text[:start] + text[end:]
old = 'permissions:\n  contents: write\n'
if text.count(old) != 1:
    raise SystemExit(f'expected one temporary write permission block, got {text.count(old)}')
path.write_text(text.replace(old, 'permissions:\n  contents: read\n', 1))
