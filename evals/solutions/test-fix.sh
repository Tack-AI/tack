cat > calc.py <<'PY'
def is_even(n):
    return n % 2 == 0


def clamp(x, lo, hi):
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x
PY
