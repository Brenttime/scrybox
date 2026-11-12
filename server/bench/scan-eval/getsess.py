import sqlite3
c = sqlite3.connect('/home/brenttime/projects/scrybox-rusttest/data/bindarr.db')
for r in c.execute("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%session%'"):
    print('table', r[0])
    cols = [x[1] for x in c.execute(f"PRAGMA table_info({r[0]})")]
    print(cols)
    for row in c.execute(f"SELECT * FROM {r[0]} LIMIT 3"):
        print(row)
for r in c.execute("SELECT id, username FROM users LIMIT 5"):
    print(r)
