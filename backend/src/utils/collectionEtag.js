// Conditional GET for the paginated collection read. Express's default ETag is a
// hash of the finished body, so a 304 still pays for the whole query + JSON
// serialization (~150-300 ms per 2000-row page; a 25k-copy collection hydrates
// in 13 pages on every Collection visit). This tag is known BEFORE the query:
// collection_read_revision (bumped by triggers on collection and card_cache)
// + user + exact query string + process boot id (a deploy that changes the
// response shape must not be answered 304 from an old tag).
const crypto = require('crypto');

const BOOT = Date.now().toString(36);

async function collectionEtag(conn, userId, originalUrl) {
  const row = await conn.get('SELECT revision FROM collection_read_revision WHERE id = 1');
  if (!row) return null;
  const q = String(originalUrl || '');
  const qh = crypto.createHash('sha1').update(q.slice(q.indexOf('?') + 1)).digest('base64url').slice(0, 16);
  return `W/"col-${BOOT}-${row.revision}-${userId}-${qh}"`;
}

// True when the request's If-None-Match already names this tag.
function matches(req, tag) {
  const inm = req.get('If-None-Match');
  if (!inm || !tag) return false;
  return inm.split(',').some(t => t.trim() === tag || t.trim() === '*');
}

module.exports = { collectionEtag, matches, BOOT };
