/* Scryfall Local — Set detail page (scryfall.com/sets/{code} replica).
 * SPA: the server routes /sets/{code} to this page; the set code comes from
 * location.pathname. Meta from /api/set/{code}; card grid from
 * /api/cards/search?q=s:CODE include:extras&order=set (Scryfall's own set
 * pages use include_extras + include_variations, so tokens/variants show).
 * Documented divergences: no price subline (no price data — type counts
 * shown instead); "spoiler" pill for unreleased sets; cards link to our
 * search page; CSV is generated client-side from the fetched cards. */
(function () {
  'use strict';

  var $ = function (id) { return document.getElementById(id); };
  var esc = function (s) {
    return String(s == null ? '' : s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  };

  // code from /sets/{code} (or ?code= for direct set.html testing). The
  // Scrybox backend proxies this portal under /scryfall-portal/*, so strip
  // that prefix first; it never appears when the server is hit directly.
  var code = null;
  var m = location.pathname.replace(/^\/scryfall-portal/, '').match(/^\/sets\/([a-zA-Z0-9]+)\/?$/);
  if (m) code = m[1].toLowerCase();
  else code = new URLSearchParams(location.search).get('code');
  if (!code || !/^[a-z0-9]{1,32}$/.test(code)) code = null;

  function failState(title, msg) {
    $('set-h1').textContent = title;
    $('set-subline1').textContent = msg || '';
    $('card-grid').innerHTML = '<div class="grid-status"><span class="big">' + esc(title) + '</span>' +
      esc(msg || '') + ' <a href="/sets.html" style="text-decoration:underline">Browse all sets</a></div>';
  }

  if (!code) { failState('No set specified', 'Try a set code like /sets/one.'); return; }

  var cards = [];          // every printing fetched (incl. extras/variants)
  var meta = null;

  function todayISO() { return new Date().toISOString().slice(0, 10); }

  function renderHeader() {
    document.title = meta.name + ' (' + meta.code.toUpperCase() + ') Card Gallery · Scryfall Local';
    $('set-h1').textContent = meta.name + ' (' + meta.code.toUpperCase() + ')';

    var isSpoiler = meta.released_at && meta.released_at > todayISO();
    var s1 = (meta.card_count || 0) + '\u00a0cards';
    if (meta.released_at) s1 += ' <span class="punct">•</span> Released\u00a0' + esc(meta.released_at);
    if (isSpoiler) s1 += '<span class="spoiler-pill on-dark" title="Not yet released — spoilers">spoiler</span>';
    $('set-subline1').innerHTML = s1;

    var bits = [];
    if (meta.set_type) bits.push(esc(meta.set_type.replace(/_/g, ' ')));
    if (meta.block) bits.push('Part of the ' + esc(meta.block) + ' block');
    if (meta.parent_set) bits.push('Token/asset group for ' + esc(meta.parent_set.toUpperCase()));
    if (meta.digital) bits.push('digital');
    if (meta.foil_only) bits.push('foil only');
    if (meta.printed_size) bits.push(meta.printed_size + ' in set');
    var s2 = bits.join(' <span class="punct">•</span> ');
    var c = meta.counts;
    if (c) {
      s2 += (s2 ? ' <span class="punct">•</span> ' : '') +
        c.creatures + ' creatures <span class="punct">•</span> ' +
        c.noncreatures + ' noncreature spells <span class="punct">•</span> ' +
        c.lands + ' lands';
    }
    $('set-subline2').innerHTML = s2;

    // symbol: icon svg (white via CSS invert), fallback code box
    if (meta.icon_svg_uri) {
      var img = $('set-symbol');
      img.onerror = function () {
        img.hidden = true;
        var fb = $('set-symbol-fallback');
        fb.textContent = meta.code;
        fb.hidden = false;
      };
      img.src = meta.icon_svg_uri;
      img.hidden = false;
    } else {
      var fb = $('set-symbol-fallback');
      fb.textContent = meta.code;
      fb.hidden = false;
    }

    // action row
    var acts = ['<a class="button-n inverted" href="/sets.html" title="Back to all sets">All Sets</a>'];
    acts.push('<button class="button-n inverted" id="btn-csv" type="button" title="Download the card list as CSV">Download CSV</button>');
    if (meta.scryfall_uri) {
      acts.push('<a class="button-n inverted" href="' + esc(meta.scryfall_uri) +
        '" target="_blank" rel="noopener" title="Open this set on scryfall.com">Scryfall ↗</a>');
    }
    acts.push('<a class="button-n inverted" href="/?q=' + encodeURIComponent('s:' + meta.code + ' include:extras') +
      '" title="Search these cards">Search</a>');
    $('set-actions').innerHTML = acts.join('');
    $('btn-csv').addEventListener('click', downloadCsv);
  }

  function cardTile(c) {
    var href = '/?q=' + encodeURIComponent('"' + c.name.replace(/"/g, '') + '"');
    var img = c.image_url
      ? '<img class="card" src="' + esc(c.image_url) + '" width="245" height="341" loading="lazy" decoding="async" title="' +
        esc(c.name) + '" alt="' + esc(c.name) + '" data-code="' + esc(c.set + ' ' + c.collector_number) + '">'
      : '<span class="card-empty"><span class="ce-code">' + esc(String(c.set).toUpperCase()) +
        ' ' + esc(c.collector_number) + '</span><span class="ce-name">' + esc(c.name) +
        '</span><small>No card art in the local index</small></span>';
    return '<div class="card-grid-item"><a class="card-grid-item-card" href="' + esc(href) + '">' +
      img + '</a></div>';
  }

  function imgFallback(e) {
    var t = e.target;
    if (t && t.tagName === 'IMG' && t.classList.contains('card')) {
      var label = t.getAttribute('data-code') || '';
      t.outerHTML = '<span class="card-empty"><span class="ce-code">' + esc(label) +
        '</span><span class="ce-name">Card art unavailable</span></span>';
    }
  }

  function renderGrid(status) {
    var html = cards.map(cardTile).join('');
    $('card-grid').innerHTML = html || ('<div class="grid-status"><span class="big">No cards found for this set.</span>' +
      'This set has no cards in the local index.</div>');
    if (status) {
      var d = document.createElement('div');
      d.className = 'grid-status';
      d.innerHTML = status;
      $('card-grid').appendChild(d);
    }
  }

  function csvCell(v) {
    v = String(v == null ? '' : v);
    return /[",\n]/.test(v) ? '"' + v.replace(/"/g, '""') + '"' : v;
  }

  function downloadCsv() {
    var header = ['Name', 'Set', 'Set Code', 'Collector Number', 'Rarity', 'Type', 'Mana Cost', 'Colors', 'Artist', 'Image URL', 'Scryfall URL'];
    var lines = [header.map(csvCell).join(',')];
    cards.forEach(function (c) {
      lines.push([
        c.name, c.set_name, String(c.set).toUpperCase(), c.collector_number,
        c.rarity, c.type_line, c.mana_cost,
        (c.colors || []).join(''), c.artist,
        c.image_url || '', (c.scryfall_uri || '').replace(/\?utm_source=api$/, '')
      ].map(csvCell).join(','));
    });
    var blob = new Blob(['\ufeff' + lines.join('\r\n')], { type: 'text/csv;charset=utf-8' });
    var a = document.createElement('a');
    a.href = URL.createObjectURL(blob);
    a.download = meta.name + ' (' + meta.code.toUpperCase() + ').cards.csv';
    document.body.appendChild(a);
    a.click();
    setTimeout(function () { URL.revokeObjectURL(a.href); a.remove(); }, 500);
  }

  document.addEventListener('error', imgFallback, true);

  // ---- fetch meta, then page through the card list ----
  if (!code) return;
  fetch('/api/set/' + encodeURIComponent(code), { headers: { Accept: 'application/json' } })
    .then(function (r) {
      if (r.status === 404) throw { notFound: true };
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    })
    .then(function (data) {
      meta = data;
      renderHeader();
      var page = 1;
      function nextPage() {
        var p = new URLSearchParams({
          q: 's:' + code + ' include:extras',
          order: 'set',
          limit: '175',
          page: String(page)
        });
        return fetch('/api/cards/search?' + p.toString(), { headers: { Accept: 'application/json' } })
          .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
          .then(function (d) {
            cards = cards.concat(d.data || []);
            if (d.has_more && page < 200) { page += 1; return nextPage(); }
            renderGrid();
          });
      }
      return nextPage().catch(function (err) {
        renderGrid();
        var d = document.createElement('div');
        d.className = 'grid-status';
        d.textContent = 'Failed to load cards: ' + (err && err.message ? err.message : err);
        $('card-grid').appendChild(d);
      });
    })
    .catch(function (err) {
      if (err && err.notFound) failState('Set “' + code + '” not found', 'No set with that code exists in the local index.');
      else failState('Failed to load set', err && err.message ? err.message : String(err));
    });
})();
