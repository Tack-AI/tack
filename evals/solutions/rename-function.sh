printf 'def compute_total(items):\n    return sum(items)\n' > billing.py
printf 'from billing import compute_total\n\nprint(compute_total([1, 2, 3]))\n' > main.py
