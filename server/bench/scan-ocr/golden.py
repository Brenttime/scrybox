import sys, json, math
sys.path.insert(0,'/app')
import numpy as np, cv2
from PIL import Image, ImageOps
import server as S
rng=np.random.default_rng(7)
y,x=np.mgrid[0:41,0:157]
base=np.stack([(x*1.3+y*2)%256,(x*0.4+40)%256,(y*5+x*0.2+90)%256],-1)
noise=rng.integers(-30,30,base.shape)
src=np.clip(base*0.6+30+noise,0,255).astype(np.uint8)
Image.fromarray(src).save('/f/src.png')
im=Image.fromarray(src)
a=ImageOps.autocontrast(im); a.save('/f/autocontrast.png')
for th in (96,128,160):
    tw=max(64,round(a.width*th/a.height)); a.resize((tw,th),S.LANCZOS).save(f'/f/lanczos_{th}.png')
    s=np.asarray(a.resize((tw,th),S.LANCZOS))
    rw=math.ceil(48*s.shape[1]/s.shape[0])
    Image.fromarray(cv2.resize(s,(rw,48))).save(f'/f/cv_{th}.png')
# names golden on a mini oracle
names=["Syr Gwyn, Hero of Ashvale","Relic Axe","Cryogen Relic","Nazgûl","Fling","Sentinel","Tri-Sentinel","Mole Man","Troop of Ponies","Panther Idol","Bone Shard","Squall","Glóin, Dwarf Emissary","Hunter's Insight","Hamletback Goliath","Bolt Bend","Cindervines","Pako, Arcane Retriever","Lightning Bolt","Lightning Axe","Lightning Helix","Shock","Shocker","A-Fling","Arcane Signet","Sol Ring","Solemn Simulacrum","Llanowar Elves","Elvish Mystic","Counterspell"]
S.ORACLE=set(names); S.EXCLUDED_NAMES.clear(); S.EXCLUDED_NAMES.add('goblin token')
S.NORM_MAP.clear(); S.NORM_LIST.clear()
for n in names:
    k=S.norm_name(n); S.NORM_MAP[k]=n; S.NORM_LIST.append(k)
qs=["Syr Gwyn, Hero of Ashvaie","SyrGwyn Hero of Ashva1e","Relic Axe","ix Cryogen Relic","Nazgul","Flinq","Fling","Sentine1","Tri-Sentine","Mole Man","Troop of Ponles","Panther ldol","Bone Shad","Squa11","Gloin, Dwarf Emissary","Hunter’s Insight","Hamletback Gol1ath","Bolt Bnd","Cindervine","Pako Arcane Retriever","Lightning Bol","Lightning","Shock","Shoker","Goblin Token","","x","ab cd Sol Ring","Solemn Simulacrun","Llanowar Elfs","Elvish Mystc","Counterspel","Arcane Sign","zzzzzzzz"]
out=[[q]+list(S.find_card_by_ocr(q)) for q in qs]
json.dump({"names":names,"excluded":["goblin token"],"cases":out},open('/f/names.json','w'),indent=1,ensure_ascii=False)
print(out)
