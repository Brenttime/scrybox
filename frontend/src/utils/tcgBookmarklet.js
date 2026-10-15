// TCGplayer "Send to Scrybox" bookmarklet.
//
// TCGplayer has no buyer API, and a pasted session cookie expires (and order
// history additionally demands a fresh password re-check). The bookmarklet
// sidesteps both: it runs INSIDE the user's own logged-in TCGplayer tab, reads
// the order off the page they are looking at, and opens Scrybox with the lines
// in the URL fragment. Nothing is stored and no credential ever leaves
// TCGplayer; the fragment is never sent to any server.
//
// The reader is deliberately structure-agnostic: TCGplayer reshuffles its markup
// often, so it keys on the one thing every order row carries, a link to
// /product/<id>. That product id is the same TCGplayer id Scryfall indexes,
// which makes the server-side match exact (printing included) rather than a
// name guess. Quantity, condition and price are read from the row's text.
//
// scrapeTcgOrderPage must stay SELF-CONTAINED (no imports, no closures): it is
// serialised with Function#toString into the bookmarklet.

export function scrapeTcgOrderPage(doc) {
  var ORDER_RE = /\b([0-9A-F]{8}-[0-9A-F]{6}-[0-9A-F]{5})\b/i;
  var COND_RE = /(Near Mint|Lightly Played|Moderately Played|Heavily Played|Damaged)(\s+(?:Holo)?Foil|\s+Etched(?:\s+Foil)?)?/i;
  // Product ids live in /product/<id> links on newer pages, and only in the
  // thumbnail URL (tcgplayer-cdn.../product/<id>_25w.jpg) on the classic Order
  // History page, whose item links are name slugs. Id 0 is the "no image"
  // placeholder, never a product.
  var PID_RE = /\/product\/(\d+)(?:[_/?#.]|$)/;
  var lines = [];
  var orders = {};
  var seenRow = [];

  function clean(t) { return String(t || '').replace(/\s+/g, ' ').trim(); }
  function textOf(n) { return n ? clean(n.innerText || n.textContent) : ''; }
  function pidOf(node) {
    var els = node.querySelectorAll('a[href*="/product/"], img');
    for (var i = 0; i < els.length; i++) {
      var e = els[i];
      var srcs = [e.getAttribute('href'), e.getAttribute('src'), e.getAttribute('data-original'), e.getAttribute('data-src')];
      for (var j = 0; j < srcs.length; j++) {
        var m = String(srcs[j] || '').match(PID_RE);
        if (m && m[1] !== '0') return m[1];
      }
    }
    return null;
  }
  function cents(t) {
    var ps = String(t || '').match(/\$\s?\d[\d,]*\.\d{2}/g) || [];
    var best = null;
    for (var q = 0; q < ps.length; q++) {
      var c = Math.round(parseFloat(ps[q].replace(/[$,\s]/g, '')) * 100);
      if (best === null || c < best) best = c;
    }
    return best;
  }
  function orderOf(el) {
    for (var n = el; n; n = n.parentElement) {
      var head = n.querySelector && n.querySelector('.orderHeader');
      var m = textOf(head || n).match(ORDER_RE);
      if (m) return m[1].toUpperCase();
    }
    return null;
  }
  function push(pid, name, set, qty, condText, priceCents, order) {
    var cm = String(condText || '').match(COND_RE);
    if (order) orders[order] = 1;
    lines.push({
      tcgplayer_product_id: pid,
      name: clean(name).slice(0, 200),
      set: clean(set).slice(0, 120),
      quantity: qty > 0 && qty < 1000 ? qty : 1,
      condition: cm ? cm[1] : null,
      is_foil: !!(cm && cm[2]) || (/\bfoil\b/i.test(condText || '') && !/non-?foil/i.test(condText || '')),
      price_cents: priceCents,
      order: order,
    });
  }

  // 1) Table rows, columns found by header text (the classic Order History page
  //    and order detail page are both ITEMS / DETAILS / PRICE / QUANTITY tables).
  var rows = doc.querySelectorAll('tr');
  for (var r = 0; r < rows.length; r++) {
    var tr = rows[r];
    var pid = pidOf(tr);
    if (!pid) continue;
    var table = tr.parentElement;
    while (table && table.tagName !== 'TABLE') table = table.parentElement;
    var heads = table ? table.querySelectorAll('th') : [];
    var col = {};
    for (var h = 0; h < heads.length; h++) {
      var ht = textOf(heads[h]).toLowerCase();
      if (/item|product|card/.test(ht) && col.item == null) col.item = h;
      else if (/detail|condition/.test(ht) && col.detail == null) col.detail = h;
      else if (/price/.test(ht) && col.price == null) col.price = h;
      else if (/qty|quantity/.test(ht) && col.qty == null) col.qty = h;
    }
    var cells = tr.querySelectorAll('td');
    if (col.price == null || col.qty == null || cells.length < heads.length) continue;
    var item = cells[col.item || 0];
    var link = item.querySelector('a[title]') || item.querySelector('a');
    var img = item.querySelector('img');
    var name = (link && (link.getAttribute('title') || textOf(link))) || (img && img.getAttribute('alt')) || '';
    var itemText = textOf(item);
    var set = name && itemText.indexOf(name) === 0 ? itemText.slice(name.length) : '';
    var qty = parseInt(textOf(cells[col.qty]).replace(/[^\d]/g, ''), 10);
    var detail = col.detail != null ? textOf(cells[col.detail]) : textOf(tr);
    seenRow.push(tr);
    push(pid, name, set, qty, detail, cents(textOf(cells[col.price])), orderOf(tr));
  }

  // 2) Fallback for non-table layouts: climb from each product link to the
  //    smallest ancestor holding only that product, and read its text.
  if (!lines.length) {
    var links = doc.querySelectorAll('a[href*="/product/"]');
    for (var k = 0; k < links.length; k++) {
      var a = links[k];
      var m = (a.getAttribute('href') || '').match(PID_RE);
      if (!m || m[1] === '0') continue;
      var el = a;
      for (var i = 0; i < 8 && el.parentElement; i++) {
        var p = el.parentElement;
        var ids = {};
        var inner = p.querySelectorAll('a[href*="/product/"]');
        for (var j = 0; j < inner.length; j++) {
          var mm = (inner[j].getAttribute('href') || '').match(PID_RE);
          if (mm) ids[mm[1]] = 1;
        }
        if (Object.keys(ids).length > 1) break;
        el = p;
        if (el.tagName === 'TR' || el.tagName === 'LI') break;
      }
      if (seenRow.indexOf(el) !== -1) continue;
      seenRow.push(el);
      var text = textOf(el);
      var nm = textOf(a);
      if (!nm) { var im = a.querySelector('img'); nm = im ? (im.getAttribute('alt') || '') : ''; }
      var qm = text.match(/(?:Qty|Quantity)\s*:?\s*(\d+)/i) || text.match(/\b(\d+)\s*[x×]\s/i) || text.match(/\s[x×]\s*(\d+)\b/i);
      var ord = orderOf(el);
      if (!ord) { var w = textOf(doc.body).match(ORDER_RE); ord = w ? w[1].toUpperCase() : null; }
      push(m[1], nm, '', qm ? parseInt(qm[1], 10) : 1, text, cents(text), ord);
    }
  }
  return { orders: Object.keys(orders), lines: lines };
}

// Runs in the TCGplayer tab. Kept tiny: scrape, then hand off by navigation
// (window.open), which works from an https page to an http LAN Scrybox where a
// fetch would be blocked as mixed content.
export function buildBookmarklet(scryboxOrigin) {
  var body =
    '(function(){' +
    'var s=(' + scrapeTcgOrderPage.toString() + ')(document);' +
    'if(!s.lines.length){alert("Scrybox read 0 cards from this page. Open TCGplayer Order History (signed in) and try again. If you are already there, TCGplayer may have changed its page; tell Totoro.");return;}' +
    'var p=encodeURIComponent(JSON.stringify({v:1,source:"tcgplayer",page:location.href.split("?")[0],orders:s.orders,lines:s.lines}));' +
    'var u=' + JSON.stringify(String(scryboxOrigin).replace(/\/+$/, '') + '/#tcgimport=') + '+p;' +
    // Phones often block or swallow popups from bookmarks: fall back to
    // navigating this tab (Back returns to TCGplayer).
    'var w=null;try{w=window.open(u,"_blank");}catch(e){}if(!w)location.href=u;' +
    '})()';
  return 'javascript:' + encodeURIComponent(body);
}

// Scrybox side: read (and forget) a payload the bookmarklet put in the URL.
export const TCG_IMPORT_HASH = '#tcgimport=';
export const TCG_IMPORT_KEY = 'scrybox_tcg_import';

export function takePendingTcgImport() {
  try {
    const raw = sessionStorage.getItem(TCG_IMPORT_KEY);
    if (!raw) return null;
    sessionStorage.removeItem(TCG_IMPORT_KEY);
    const data = JSON.parse(raw);
    return data && Array.isArray(data.lines) && data.lines.length ? data : null;
  } catch { return null; }
}
export function hasPendingTcgImport() {
  try { return !!sessionStorage.getItem(TCG_IMPORT_KEY); } catch { return false; }
}

export function readTcgImportHash(hash) {
  if (typeof hash !== 'string' || !hash.startsWith(TCG_IMPORT_HASH)) return null;
  try {
    const data = JSON.parse(decodeURIComponent(hash.slice(TCG_IMPORT_HASH.length)));
    if (!data || !Array.isArray(data.lines)) return null;
    const lines = data.lines
      .filter((l) => l && /^\d{1,10}$/.test(String(l.tcgplayer_product_id || '')))
      .slice(0, 1000);
    if (!lines.length) return null;
    return { orders: Array.isArray(data.orders) ? data.orders.map(String).slice(0, 50) : [], lines };
  } catch {
    return null;
  }
}
