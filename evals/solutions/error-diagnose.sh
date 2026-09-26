python3 - <<'PY'
path = 'greeter.py'
src = open(path).read()
open(path, 'w').write(src.replace('shoutt', 'shout'))
PY
