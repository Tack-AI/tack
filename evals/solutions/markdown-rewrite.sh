python3 - <<'PY'
import re
lines = open('doc.md').read().splitlines(keepends=True)
out = []
for line in lines:
    stripped = line.rstrip('\n')
    m = re.match(r'^(#{2,}) (.*)$', stripped)
    if m:
        stripped = m.group(1)[1:] + ' ' + m.group(2)
    stripped = re.sub(r'(?<![<(])(https?://\S+)', r'<\1>', stripped)
    out.append(stripped + ('\n' if line.endswith('\n') else ''))
open('doc.md', 'w').write(''.join(out))
PY
