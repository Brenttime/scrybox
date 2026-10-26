#!/usr/bin/env bash
# Regression suite for the local-scryfall search API (:8420).
#   - fuzzy typo regressions (plain-word queries with spelling errors)
#   - exact ranking guard: q=dark ritual must put "Dark Ritual" first
#   - grammar/operator suite (c=, c>, c!, ci>, cmc ranges, phrases, artist)
#     stays green AND fast (bitmap paths <5ms; substring-scan paths <10ms)
# Run with the server up:  ./tests.sh [base-url]
BASE="${1:-http://127.0.0.1:8420}"
PASS=0 FAIL=0

# search <q> [limit] -> json body
search() {
  curl -s "$BASE/api/search" --get --data-urlencode "q=$1" --data-urlencode "limit=${2:-50}"
}

report() {
  local label="$1" st="$2" detail="$3"
  if [ "$st" = OK ]; then PASS=$((PASS+1)); printf 'PASS  %-30s %s\n' "$label" "$detail"
  else FAIL=$((FAIL+1)); printf 'FAIL  %-30s %s\n' "$label" "$detail"; fi
}

# check <label> <q> <want>: total>0 and <want> appears in name OR type_line
# of the top results (case-insensitive substring).
check() {
  local label="$1" q="$2" want="$3" out
  out=$(search "$q" | WANT="$want" python3 -c '
import json,sys,os
want=os.environ["WANT"].lower()
try: d=json.load(sys.stdin)
except Exception as e: print("BADJSON",e); sys.exit()
data=d.get("data",[])
ok=d.get("total",0)>0 and any(want in (c["name"]+" "+c.get("type_line","")).lower() for c in data)
print("OK" if ok else "MISS total=%s top=%s"%(d.get("total"),[c["name"] for c in data[:3]]))')
  report "$label" "${out%% *}" "q=$q  ${out#* }"
}

# check_first <label> <q> <name>: first result is exactly <name>
check_first() {
  local label="$1" q="$2" want="$3" first
  first=$(search "$q" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["data"][0]["name"] if d.get("data") else "<none>")')
  if [ "$first" = "$want" ]; then report "$label" OK "q=$q first=$first"
  else report "$label" FAIL "q=$q first=$first want=$want"; fi
}

# check_all <label> <q> <py-expr c->: total>0 and every returned card satisfies
# the python predicate expression (variable c = card dict).
check_all() {
  local label="$1" q="$2" expr="$3" out
  out=$(search "$q" 500 | EXPR="$expr" python3 -c '
import json,sys,os
expr=os.environ["EXPR"]
d=json.load(sys.stdin)
data=d.get("data",[])
try: ok=d.get("total",0)>0 and all(eval(expr) for c in data)
except Exception as e: print("ERR",e); sys.exit()
print("OK" if ok else "BAD total=%s sample=%s"%(d.get("total"),
  [(c["name"],c.get("colors"),c.get("cmc"),c.get("type_line")) for c in data[:2]]))')
  report "$label" "${out%% *}" "q=$q  ${out#* }"
}

# check_took <label> <q> <max_ms>: min took_ms over 3 runs <= max_ms
check_took() {
  local label="$1" q="$2" max="$3" best out
  best=99999
  for _ in 1 2 3; do
    t=$(search "$q" 1 | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("took_ms",99999) if d.get("total",-1)>0 else 99999)')
    best=$(python3 -c "print(min($best,$t))")
  done
  out=$(python3 -c "print('OK' if $best<=$max else 'SLOW')")
  report "$label" "$out" "q=$q took_ms=$best (max $max)"
}

echo "== fuzzy typo regressions (plain words)"
check "fuzzy black lotsu"   "black lotsu"  "Black Lotus"
check "fuzzy lite bolt"     "lite bolt"    "Lightning Bolt"
check "fuzzy dark rital"    "dark rital"   "Dark Ritual"
check "fuzzy planezwalker"  "planezwalker" "Planeswalker"
check "fuzzy tasigur"       "tasigur"      "Tasigur, the Golden Fang"
check_took "fuzzy fast lotsu" "black lotsu" 5
check_took "fuzzy fast pw"    "planezwalker" 5

echo "== exact ranking must not regress"
check_first "dark ritual first"  "dark ritual"    "Dark Ritual"
check "lightning bolt present"   "lightning bolt" "Lightning Bolt"
check "black lotus present"      "black lotus"    "Black Lotus"

echo "== operator suite patterns (fields must actually filter)"
check_all "c=b"                  "c=b"                  "'B' in c['colors']"
check_all "c=wub exact"          "c=wub"                "sorted(c['colors'])==['B','U','W']"
check_all "c>r (include R)"      "c>r"                  "'R' in c['colors']"
check_all "c!r (exclude R)"      "c!r"                  "'R' not in c['colors']"
check_all "ci>=gw"               "ci>=gw"               "set(['G','W'])<=set(c['color_identity'])"
check_all "cmc=2-4 range"        "cmc=2-4"              "2<=c['cmc']<=4"
check_all "cmc>6"                "cmc>6"                "c['cmc']>6"
check_all "is:planeswalker"      "is:planeswalker"      "'planeswalker' in c['type_line'].lower()"
check_all "artist:jeff"          "artist:jeff"          "'jeff' in c['artist'].lower()"
check_all "combo c=bu cmc>3 i"   "c=bu cmc>3 t:instant" "set(['B','U'])<=set(c['colors']) and c['cmc']>3 and 'instant' in c['type_line'].lower()"
check "phrase"                   '"deals 3 damage" lightning' "Lightning Bolt"

echo "== grammar queries stay fast server-side"
check_took "c=wub"                "c=wub"                    5
check_took "c>2"                  "c>2"                      5
check_took "c!r"                  "c!r"                      5
check_took "ci>=gw"               "ci>=gw"                   5
check_took "cmc=2-4"              "cmc=2-4"                  5
check_took "cmc>6"                "cmc>6"                    5
check_took "is:creature c=r"      "is:creature c=r"          5
check_took "phrase"               '"deals 3 damage"'         5
check_took "artist:jeff (scan)"   "artist:jeff"              10

# check_total <label> <q> <want>: total == want exactly
check_total() {
  local label="$1" q="$2" want="$3" out
  out=$(search "$q" 1 | WANT="$want" python3 -c '
import json,sys,os
d=json.load(sys.stdin); want=int(os.environ["WANT"])
print("OK" if d.get("total")==want else "BAD total=%s want=%s"%(d.get("total"),want))')
  report "$label" "${out%% *}" "q=$q  ${out#* }"
}

# check_warn0 <label> <q>: total==0 and at least one warning (ignored-term mirror)
check_warn0() {
  local label="$1" q="$2" out
  out=$(search "$q" 1 | python3 -c '
import json,sys
d=json.load(sys.stdin)
ok=d.get("total")==0 and d.get("warnings")
print("OK" if ok else "BAD total=%s warn=%s"%(d.get("total"),d.get("warnings")[:1]))')
  report "$label" "${out%% *}" "q=$q  ${out#* }"
}

# check_400 <label> <q>: HTTP 400 (mirrors upstream invalid_search_syntax
# rejects for closed-enum values: id: junk letters, bad k:/frame:/r: values)
check_400() {
  local label="$1" q="$2" st out
  st=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/api/search" --get --data-urlencode "q=$q" --data-urlencode limit=1)
  if [ "$st" = 400 ]; then out=OK; else out="BAD http=$st"; fi
  report "$label" "${out%% *}" "q=$q  ${out#* }"
}

# check_eq <label> <q1> <q2>: same total for both queries
check_eq() {
  local label="$1" q1="$2" q2="$3" out
  out=$( { search "$q1" 1; echo; search "$q2" 1; } | python3 -c '
import json,sys
a=json.loads(sys.stdin.readline()); b=json.loads(sys.stdin.readline())
print("OK" if a.get("total")==b.get("total") and a.get("total",0)>0 else "BAD %s!=%s"%(a.get("total"),b.get("total")))')
  report "$label" "${out%% *}" "$q1 vs $q2  ${out#* }"
}

echo "== id: (card colour abbreviation — real semantics: oracle colour identity)"
# live-calibrated golden case (verified against scryfall.com full-set diff, Oct 2026)
check_total "otag:ramp id:rb == 874"  "otag:ramp id:rb"     874
check_total "otag:draw id:rb == 1331" "otag:draw id:rb"     1331
check_all "id:rb ci⊆{R,B}"            "id:rb"               "set(c['color_identity']) <= {'R','B'}"
check_all "id:2 exactly 2 colours"    "id:2"                "len(c['color_identity'])==2"
check_total "id:4 == 23 (five-colour-adjacent)" "id:4"      23
check_eq    "id:0 == id:c (colorless)" "id:0"               "id:c"
check_eq    "id:rb == id:br"            "id:rb"             "id:br"
check_eq    "id:w id:u == id:c"         "id:w id:u"         "id:c"
check_eq    "id:RB == id:rb (case)"     "id:RB"             "id:rb"
# values Scryfall ignores (400 'all terms ignored') => warning + 0, never a crash
check_400    "id:zz rejected"            "id:zz"
check_400    "id:jb rejected"            "id:jb"
check_warn0 "id:rc mixed-c ignored"     "id:rc"
check_warn0 "id:6 digit-range"          "id:6"
check_warn0 "id:11 multi-digit"         "id:11"
check_warn0 "cardid: not an operator"   "cardid:rb"
check_took "id:rb fast"                 "id:rb"              10

echo
echo "passed=$PASS failed=$FAIL"
[ "$FAIL" -eq 0 ]
