python3 - <<'PY'
import sqlite3
con = sqlite3.connect('shop.db')
rows = con.execute(
    'select customer, sum(amount) from orders group by customer '
    'order by sum(amount) desc, customer asc'
).fetchall()
open('result.txt', 'w').write(''.join('%s %d\n' % (c, t) for c, t in rows))
PY
