/* Scryfall Local — Sets page (scryfall.com/sets replica).
 * Fetches /api/sets with server-side q/type/order params (mirror of the
 * site's ?q=&type=&order= handling), keeps controls ↔ URL in sync via
 * history.replaceState so links are shareable, and links each row to the
 * set detail page /sets/{code}.
 * Documented divergences from scryfall.com/sets:
 *  - English-only: the lang dropdown is kept but visually hidden and only
 *    matches en (dataset has no other languages); non-en selections show 0.
 *  - Text filter input added (Scryfall's /sets has none).
 *  - "spoiler" pill marks sets whose released_at is in the future.
 */
(function () {
  'use strict';

  var $ = function (id) { return document.getElementById(id); };
  var esc = function (s) {
    return String(s == null ? '' : s).replace(/[&<>"]/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c];
    });
  };

  // Same options + order as scryfall.com/sets' "of type" dropdown.
  var TYPE_OPTS = [
    ['cube', 'Cube'], ['alchemy', 'Alchemy'], ['archenemy', 'Archenemy'],
    ['arsenal', 'Arsenal'], ['box', 'Box'], ['commander', 'Commander'],
    ['core', 'Core'], ['draft_innovation', 'Draft Innovation'],
    ['duel_deck', 'Duel Deck'], ['eternal', 'Eternal'], ['expansion', 'Expansion'],
    ['from_the_vault', 'From The Vault'], ['funny', 'Funny'],
    ['masterpiece', 'Masterpiece'], ['masters', 'Masters'],
    ['memorabilia', 'Memorabilia'], ['minigame', 'Minigame'],
    ['planechase', 'Planechase'], ['premium_deck', 'Premium Deck'],
    ['promo', 'Promo'], ['spellbook', 'Spellbook'], ['starter', 'Starter'],
    ['token', 'Token'], ['treasure_chest', 'Treasure Chest'], ['vanguard', 'Vanguard']
  ];

  var PILL_LANGS = ['es', 'fr', 'de', 'it', 'pt', 'ja', 'ko', 'ru', '汉语', '漢語'];

  // Build the "of type" select.
  (function () {
    var sel = $('type');
    TYPE_OPTS.forEach(function (o) {
      var op = document.createElement('option');
      op.value = o[0];
      op.textContent = o[1];
      sel.appendChild(op);
    });
  })();

  // ---- deep-link params: sets.html?q=one&type=box,core&order=block ----
  (function () {
    var params = new URLSearchParams(location.search);
    var t = params.get('type');
    if (t) {
      var first = t.split(',')[0].trim();
      var opt = $('type').querySelector('option[value="' + first.replace(/["\\]/g, '\\$&') + '"]');
      if (opt) $('type').value = first;
      else $('type').dataset.raw = t; // multi-type deep link: server filters raw value
    }
    var o = params.get('order');
    if (o && $('order').querySelector('option[value="' + o + '"]')) $('order').value = o;
    var q = params.get('q');
    if (q) $('q-text').value = q;
  })();

  function syncUrl() {
    var p = new URLSearchParams();
    var q = $('q-text').value.trim();
    if (q) p.set('q', q);
    var t = $('type').value;
    if (t) p.set('type', t);
    var o = $('order').value;
    if (o && o !== 'date') p.set('order', o);
    var lang = $('lang').value;
    if (lang) p.set('lang', lang);
    var qs = p.toString();
    history.replaceState(null, '', location.pathname + (qs ? '?' + qs : ''));
  }

  function todayISO() {
    return new Date().toISOString().slice(0, 10);
  }

  function placeholder(code) {
    var ch = (code || 'M').charAt(0);
    return '<span class="set-art set-art-placeholder" aria-hidden="true">' + esc(ch) + '</span>';
  }

  function rowHtml(s) {
    var href = '/sets/' + encodeURIComponent(s.code);
    var thumb = s.icon_svg_uri
      ? '<img class="set-art" data-fallback="' + esc(s.sample_image || '') + '" src="' + esc(s.icon_svg_uri) +
        '" width="20" height="20" loading="lazy" decoding="async" alt="">'
      : (s.sample_image
        ? '<img class="set-art" data-fallback="" src="' + esc(s.sample_image) +
          '" width="20" height="20" loading="lazy" decoding="async" alt="">'
        : placeholder(s.code));
    var spoiler = s.released_at && s.released_at > todayISO()
      ? '<span class="pillbox-item spoiler-pill" title="Unreleased — spoilers">spoiler</span>'
      : '';
    var pills = '<span class="pillbox"><a class="pillbox-item" href="' + href + '">en</a>' +
      PILL_LANGS.map(function (l) {
        return '<span aria-hidden="true" class="pillbox-item disabled">' + esc(l) + '</span>';
      }).join('') + '</span>';
    return '<tr>' +
      '<td class="flexbox"><a href="' + href + '">' + thumb +
        '<span class="set-name-text">' + esc(s.name) + '</span>' +
        '<small>' + esc(String(s.code).toUpperCase()) + '</small>' + spoiler + '</a></td>' +
      '<td><a tabindex="-1" href="' + href + '">' + s.card_count + '</a></td>' +
      '<td><a tabindex="-1" href="' + href + '">' + esc(s.released_at || '') + '</a></td>' +
      '<td class="col-lang">' + pills + '</td></tr>';
  }

  var inflight = null;

  function load() {
    syncUrl();
    var p = new URLSearchParams();
    var q = $('q-text').value.trim();
    if (q) p.set('q', q);
    var t = $('type').value || (($('type').dataset.raw) || '');
    if (t) p.set('type', t);
    var o = $('order').value;
    if (o) p.set('order', o);
    $('sets-count').textContent = '…';
    $('sets-body').innerHTML = '<tr><td colspan="4" class="sets-status">Loading…</td></tr>';
    if (inflight) inflight.abort();
    inflight = new AbortController();
    fetch('/api/sets?' + p.toString(), { signal: inflight.signal })
      .then(function (r) { if (!r.ok) throw new Error('HTTP ' + r.status); return r.json(); })
      .then(function (data) {
        var rows = data.sets || [];
        var lang = $('lang').value;
        if (lang && lang !== 'en') rows = []; // English-only dataset (documented)
        $('sets-count').textContent = String(rows.length);
        var html = rows.map(rowHtml).join('');
        var empty = lang && lang !== 'en'
          ? 'No ' + esc(lang) + ' data in this local index — the card dump is English-only.'
          : 'No sets match these filters.';
        $('sets-body').innerHTML = html || ('<tr><td colspan="4" class="sets-status">' + empty + '</td></tr>');
        armLazyImgs();
      })
      .catch(function (err) {
        if (err && err.name === 'AbortError') return;
        $('sets-count').textContent = '0';
        $('sets-body').innerHTML = '<tr><td colspan="4" class="sets-status">Failed to load sets: ' +
          esc(err && err.message ? err.message : err) + '</td></tr>';
      });
  }

  ['lang', 'order', 'type'].forEach(function (id) {
    $(id).addEventListener('change', load);
  });
  var debounce;
  $('q-text').addEventListener('input', function () {
    clearTimeout(debounce);
    debounce = setTimeout(load, 180);
  });

  // Icon chain: icon svg -> sample card jpg -> CSS placeholder (per level).
  document.addEventListener('error', function (e) {
    var t = e.target;
    if (t && t.tagName === 'IMG' && t.classList && t.classList.contains('set-art')) {
      var fb = t.getAttribute('data-fallback');
      if (fb && t.src !== fb) {
        t.setAttribute('data-fallback', '');
        t.src = fb;
        return;
      }
      t.outerHTML = placeholder(t.getAttribute('data-code') || '');
    }
  }, true);

  // Lazy-load safety net (see previous revision's note).
  function armLazyImgs() {
    if (!('IntersectionObserver' in window)) return;
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (en) {
        if (en.isIntersecting) {
          en.target.loading = 'eager';
          io.unobserve(en.target);
        }
      });
    }, { rootMargin: '300px 0px' });
    document.querySelectorAll('img.set-art[lazy],[loading="lazy"]').forEach(function (i) { io.observe(i); });
  }

  load();
})();
