cat > math_utils.py <<'PY'
def total(a, b, tax):
    return (a + b) * (1 + tax)
PY
cat > app.py <<'PY'
from math_utils import total

print(total(100, 200, 0.1))
PY
cat > report.py <<'PY'
from math_utils import total


def invoice():
    return total(50, 25, 0.1)
PY
