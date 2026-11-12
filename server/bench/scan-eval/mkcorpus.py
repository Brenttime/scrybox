import numpy as np, sqlite3, json, urllib.request, os, sys
out = os.path.dirname(os.path.abspath(__file__)) + '/corpus'
os.makedirs(out, exist_ok=True)
d = np.load('/home/brenttime/projects/scrybox-rusttest/data/models/milo-mtg.npz')
ids = set(str(x).replace('_back', '') for x in d['card_ids'])
c = sqlite3.connect('/home/brenttime/projects/scrybox-rusttest/data/bindarr.db')
cols = [r[1] for r in c.execute("PRAGMA table_info(card_cache)")]
print('cols:', cols)
rows = c.execute("SELECT id, name, set_id, image_url FROM card_cache "
                 "WHERE image_url IS NOT NULL AND image_url != '' LIMIT 20000").fetchall()
picks = [r for r in rows if r[0].startswith('mtg-') and r[0][4:] in ids][:8]
print('picked', len(picks))
manifest = []
for i, (cid, name, setc, uri) in enumerate(picks):
    print(i, name, setc, uri)
    try:
        urllib.request.urlretrieve(uri, f'{out}/card{i}.png')
        manifest.append({'id': cid, 'name': name, 'set': setc, 'file': f'card{i}.png'})
    except Exception as e:
        print('FAIL', e)
json.dump(manifest, open(f'{out}/manifest.json', 'w'), indent=1)
