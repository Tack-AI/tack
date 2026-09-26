python3 - <<'PY'
text = open('settings.json').read()
open('settings.json', 'w').write(text.replace('"log_level": "info"', '"log_level": "debug"'))
PY
