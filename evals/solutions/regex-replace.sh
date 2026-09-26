python3 - <<'PY'
import re
text = open('dates.txt').read()
text = re.sub(r'(\d{2})/(\d{2})/(\d{4})', r'\3-\1-\2', text)
open('dates.txt', 'w').write(text)
PY
