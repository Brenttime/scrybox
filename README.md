<div align="center">

<img src="frontend/public/logo.svg" width="120" height="120" alt="" />

# Scrybox

**Self-hosted Magic: The Gathering collection manager — a ManaBox replacement you run yourself.**

Identify cards with your phone camera, track prices, and pull decks back out of your collection again.

[![CI](https://img.shields.io/github/actions/workflow/status/Brenttime/scrybox/docker-build.yml?branch=main&label=CI&logo=github)](https://github.com/Brenttime/scrybox/actions/workflows/docker-build.yml)
[![Docker image](https://img.shields.io/badge/ghcr.io-scrybox-2496ED?logo=docker&logoColor=white)](https://github.com/Brenttime/scrybox/pkgs/container/scrybox)
[![License: MIT](https://img.shields.io/github/license/Brenttime/scrybox?color=blue)](LICENSE)
[![Stars](https://img.shields.io/github/stars/Brenttime/scrybox?style=flat&logo=github)](https://github.com/Brenttime/scrybox/stargazers)
[![Issues](https://img.shields.io/github/issues/Brenttime/scrybox)](https://github.com/Brenttime/scrybox/issues)

[![Ko-fi](https://img.shields.io/badge/Ko--fi-Support%20Scrybox-ff5e5b?logo=ko-fi&logoColor=white)](https://ko-fi.com/M4M31KYZ9Y)

[Fork differences](#about-this-fork) · [Screenshots](#screenshots) · [Install](#install) · [Features](#features) · [How it works](PROJECT.md) · [Report a bug](https://github.com/Brenttime/scrybox/issues/new)

</div>

---

## About this fork

Scrybox is a **hard fork** of
[`thenotoriousJeremy/bindarr`](https://github.com/thenotoriousJeremy/bindarr),
narrowed to **Magic: The Gathering only**. Its goal is to replace ManaBox for
people who want their collection, scans, decks and lists on their own server,
with no subscription and no cloud account.

It no longer tracks upstream: compatible scanner, performance and correctness
fixes may still be ported by hand, but upstream features that broaden the app
into a general trading-card manager are deliberately left out. Some internal
names (the `bindarr.db` database file, browser storage keys, the
`com.bindarr.app` mobile identifier) keep their original spelling so existing
installs, saved logins and app-store builds keep working.

Compared with upstream, this fork:

- supports **Magic: The Gathering only** — no Pokémon, Lorcana, game picker, or
  multi-game provider layer;
- removes physical storage management (locations, compartments, and binder/box
  placement), grading/slab lookup, the standalone Notes page, and the collection
  wishlist mode;
- keeps per-card notes and separate ManaBox-style **Card Lists** for wishlists,
  buylists, and missing-card lists;
- replaces the standalone card-search page with one collection search box that
  understands names, set/collector numbers, and Scryfall syntax, answering from
  the local cache whenever possible;
- keeps Admin and Moxfield controls inside **Settings** rather than as top-level
  navigation; and
- adds MTG-focused deck checkout, precon import, collection registration,
  inventory-integrity safeguards, and large-catalog search optimizations.

Those boundaries are product decisions, not missing upstream work.

---

## Screenshots

<p align="center">
  <img src="docs/screenshots/dashboard.webp" alt="Dashboard: net worth over time, gain, rarity breakdown, most valuable cards and top sets" width="100%" />
</p>

| Collection | Deck |
|---|---|
| <img src="docs/screenshots/collection.webp" alt="Collection grid with card art, rarity, condition and price" /> | <img src="docs/screenshots/deck.webp" alt="Deck view showing owned and missing cards, color balance and deck value" /> |

<p align="center">
  <img src="docs/screenshots/mobile-dashboard.webp" alt="Dashboard on a phone" width="260" />
  &nbsp;&nbsp;
  <img src="docs/screenshots/mobile-collection.webp" alt="Collection on a phone" width="260" />
</p>

<sub>Screenshots use the built-in demo data.</sub>

---

## Features

- **Camera scanning** — photograph a card and the server identifies it from the image alone.
- **Deck checkout** — reserve a deck's cards and see, card by card, how many you own, how many other checked-out decks have locked, and how many are still missing; the same checklist in reverse when you put them back.
- **Card lists** — ManaBox-style wishlists, buylists and missing-card lists: named lists of cards you track without owning, with per-card wanted quantities, an owned-vs-missing readout, and the same text export the collection uses (`qty Name` / `qty Name (SET) num`). Create one from a pasted list or grow it by searching.
- **Search and bulk add** — search or browse a whole set with multi-select; pin a set and add by collector number one keystroke at a time.
- **Dashboard** — collection value, 7/30-day trends, rarity and type breakdowns, set completion.
- **Cards in 11 languages** — search, scan and record Japanese, Korean, Chinese, German, French, Spanish, Italian, Portuguese and Russian printings. A copy references the printing it actually is, so it shows the name and artwork on the card while staying searchable by its English name.
- **Exports and API** — CSV (TCGplayer-compatible) or JSON, a text card list (`qty Name` or `qty Name (SET) num`, for ManaBox / Moxfield / TCGplayer Mass Entry), plus a read-only API key for reading net worth from elsewhere.
- **Multi-user** — session-token auth, admin panel for users and roles, optional public share links.
- **11 UI languages**, community-translated.

Architecture, the scan pipeline, and the data model are in [PROJECT.md](PROJECT.md).

---

## Install

### Docker (recommended)

No clone, no build. Create a `docker-compose.yml`:

```yaml
services:
  scrybox:
    image: ghcr.io/brenttime/scrybox:edge
    container_name: scrybox
    restart: unless-stopped
    ports:
      - "3001:3001"   # HTTP  — point a reverse proxy here
      - "3443:3443"   # HTTPS — use this directly if you have no proxy (scanning needs it)
    environment: {}
      # All optional — see the table below.
      # - PUBLIC_BASE_URL=
      # - DEFAULT_ADMIN_PASSWORD=
      # - TRUST_PROXY=1
    volumes:
      - bindarr-data:/app/database

volumes:
  bindarr-data:
```

Start it:

```bash
docker compose up -d
```

Open `http://localhost:3001`. The first visit asks you to set a password for the
`admin` account and creates it right there — no password is ever written to the
logs. (If you set `DEFAULT_ADMIN_PASSWORD` instead, that same `admin` account is
created at startup and the first visit is a normal login screen.)

You can change the password later in Settings. Everything (database, backups, scan models and catalogs, TLS cert) lives in the `bindarr-data` volume.

Update with `docker compose pull && docker compose up -d`. Your volume is untouched.

To build from source instead, clone the repo and run `docker compose up -d` — the bundled [`docker-compose.yml`](docker-compose.yml) uses `build:` rather than `image:`.

#### Which port to use

Both ports serve the same app and the same database. Only the transport differs.

| Port | Use it when |
| --- | --- |
| `3001` (HTTP) | You have a reverse proxy (Caddy, NPM, Traefik, Tailscale Serve) terminating TLS in front, or you're on the host itself at `http://localhost:3001`. |
| `3443` (HTTPS) | You have no proxy and want to reach Scrybox from a phone. **Card scanning only works here** — browsers refuse camera access over plain HTTP to anything but `localhost`. |

Publish only the one you use. Behind a proxy, drop the `3443` line and set `TRUST_PROXY=1`.

The HTTPS certificate is self-signed and generated on first start into the volume, so your browser warns once per device (**Advanced → Proceed**; iOS Safari: **Show Details → Visit this website**). Mount a real certificate and set `SSL_CERT_PATH` + `SSL_KEY_PATH` to skip the warning.

#### Image tags

| Tag | Points at |
| --- | --- |
| `edge` | newest `main` commit — use this until the first MTG-only release |
| `latest` | newest tagged release, once one has been published |
| `sha-<short>` | one exact commit |

#### Environment variables

All optional.

| Variable | Default | Purpose |
| --- | --- | --- |
| `PORT` | `3001` | HTTP port. |
| `HTTPS_PORT` | `3443` in the image | HTTPS port. Set to `""` for HTTP only. |
| `SSL_CERT_PATH` / `SSL_KEY_PATH` | — | Your own certificate instead of the generated self-signed one. |
| `DB_PATH` | `/app/database/bindarr.db` | SQLite file location. |
| `DEFAULT_ADMIN_PASSWORD` | — | Create the `admin` account with this password at startup instead of letting the first browser visit create the owner account. Only applied while the `users` table is empty — changing it later does nothing to an existing account. |
| `PUBLIC_BASE_URL` | — | External URL behind a proxy, e.g. `https://cards.example.com`. Used for share links and auto-allowed as a CORS origin, so proxied logins work with just this. Also editable in the Admin panel. |
| `CORS_ORIGIN` | — | Extra allowed origins, comma-separated. Localhost and private-LAN origins are always allowed. |
| `ALLOW_REGISTRATION` | unset | `true` allows self-registration. Unset means invite-only: admins create accounts. |
| `TRUST_PROXY` | — | Number of proxy hops (usually `1`) when a reverse proxy terminates TLS, so rate limiting sees the real client IP. |
| `CV_MODEL_DIR` | `/app/database/models` in the image | Where the scan models and catalogs live. Must be on persistent storage, or an image update discards every catalog you built. |
| `CV_SCAN_GAP` | `0.10` | How far a match must stand above its runner-ups before it counts as an answer rather than "this card is not in the catalog". Lower accepts more guesses. |
| `BACKUP_INTERVAL_HOURS` | `24` | Automatic database snapshot interval. `0` disables. |
| `BACKUP_KEEP_LAST` | `10` | How many snapshots to retain. |

`GET /api/health` returns `200 {"status":"ok"}` with no auth and backs the image's `HEALTHCHECK`.

### Prebuilt server binary

If you'd rather not run Docker, every release ships a self-contained server. Download it from the [latest release](https://github.com/Brenttime/scrybox/releases/latest), unpack, run, then open `http://localhost:3001`.

| OS | File | Run |
|----|------|-----|
| Windows | `Scrybox-Server-windows-x64.zip` | unzip, double-click `scrybox-server.exe` |
| Linux | `Scrybox-Server-linux-x64.tar.gz` | `tar xzf`, then `chmod +x scrybox-server && ./scrybox-server` |
| macOS (Apple Silicon) | `Scrybox-Server-macos-arm64.tar.gz` | `tar xzf`, then `chmod +x scrybox-server && ./scrybox-server` |

The first visit in a browser asks you to create the owner account. The SQLite file is created next to the binary. To set variables from the table above, create `app/backend/.env` before the first run — HTTPS is off by default here, so add `HTTPS_PORT=3443` if you want to scan from a phone.

### Mobile apps

`Scrybox-Android.apk` is attached to each release (allow "install from unknown sources"). iOS goes out through TestFlight. Both talk to a Scrybox server, so install one of the above first and point the app at it.

---

## Card scanning

Scanning is image-only — no OCR, no typing. Two things have to be in place, and
neither ships inside the app:

**1. The models.** Two small neural networks (~8 MB together) find the card in
the frame and turn its artwork into a fingerprint. The detector is MIT and the
embedding model is AGPL-3.0-only; Scrybox itself is MIT, so the models are fetched
deliberately rather than bundled:

```bash
docker exec scrybox node scripts/fetch-models.mjs
```

(Running from source or the prebuilt binary: `node scripts/fetch-models.mjs` in
`backend/`.) Restart afterwards. Until they are there, the server says so at
startup and scanning returns a clear error instead of guesses.

**2. A catalog.** The fingerprint has to be compared against something. Build one
per language from **Settings → Scan catalogs**: it fingerprints the artwork of
every card in your local card cache — hours for a full English build, and about
5 MB of output per 10,000 cards. Progress is live, stopping keeps what it has,
and resuming reuses it. `--catalogs` on the fetch command above grabs a
prebuilt English catalog instead, which answers immediately but only resolves
cards your install has already seen.

Scanning is then the same whether or not you tell it which set you are feeding.
Naming the set just restricts the comparison to that set's cards, which is faster
and more accurate, and needs no preparation.

Coverage is only as good as the card data available for a language: the panel
shows what a catalog holds against what the provider claims exists. A card
outside the catalog is reported as missing rather than guessed at, with the
nearest matches offered underneath.

The pipeline, its accuracy numbers, and the measurement harness are documented in
[PROJECT.md](PROJECT.md#image-identification-pipeline).

---

## Card values

Prices come from Scryfall, which quotes two marketplaces: TCGplayer's USD price
for printings it lists, and Cardmarket's EUR price for everything else (most
non-English printings). Prices are stored in the currency they were quoted in
and never converted — an exchange rate is a live number Scrybox has no source
for — so each card shows its own symbol (`$4.50`, `€4.50`) and the card
inspector names the marketplace. Collection totals sum the currencies as-is;
`currencies` in the API response says when a total is mixed.

A price only exists once something fetched it. The daily price sweep covers the
cards you own or run in decks, so browsing a set you own nothing in shows
`0.00` until a card from it lands in your collection.

## API access

For reading your collection from somewhere else — a finance tracker, a dashboard widget, a script — generate a key under **Settings → API Keys** and send it as a Bearer token:

```bash
curl -H "Authorization: Bearer <key>" http://localhost:3001/api/stats/networth
```

```json
{ "totalValue": 4210.55, "totalSpent": 2980.00, "gain": 1230.55, "gainPct": 41.3,
  "totalCards": 812, "uniqueEntries": 604,
  "currencies": ["USD"], "asOf": "2026-08-17T17:55:40.898Z" }
```

The key never expires and is **read-only**: any request that is not a GET is refused, and admin endpoints are refused outright. `/api/stats` returns the full dashboard payload if you want the breakdowns too, and `/api/collection` returns every card. More than one entry in `currencies` means the collection mixes cards quoted in different currencies and the total sums them as-is.

Revoking is one click in the same panel; anything using the old key stops immediately.

## Backup and restore

Everything is in one SQLite file (`DB_PATH`, in the `bindarr-data` volume under Docker).

Scrybox snapshots the database itself every 24 hours into a `backups/` folder beside it, keeping the last 10. That runs while the server is live and needs no downtime.

For an off-box copy, stop the container first so the WAL is checkpointed:

```bash
docker run --rm -v bindarr-data:/data -v "$PWD":/backup alpine cp /data/bindarr.db /backup/
```

Restore by dropping the file back into the volume with the container stopped. Individual users can also export and re-import their own collection as CSV or JSON from the app.

**Lost the admin password?** There's no self-service reset, and `DEFAULT_ADMIN_PASSWORD` will not change an existing account — it is only used when no accounts exist. Delete the database file to start fresh and create the owner account again.

> **Upgrading from before v1.5.0:** the database file was renamed `pokemon_cards.db` → `bindarr.db`. First start renames it automatically, sidecars included. Old `pokemon_cards.*.bak` backups still restore.

---

## Development

Node 18+ and npm 9+.

```bash
npm run install:all
npm run dev
```

Frontend at `https://localhost:5173` (self-signed HTTPS, so the camera works), backend at `http://localhost:3001`.

To test scanning from a phone, put it on the same Wi-Fi and open `https://<your-computer-ip>:5173`, then accept the certificate warning (**Advanced → Proceed**).

Tests: `npm test`. Frontend lint (matches CI, fails on any warning): `cd frontend && npm run lint`. Code layout and conventions: [PROJECT.md](PROJECT.md).

---

## Translating

Scrybox speaks English, Brazilian Portuguese, French, German, Italian, Japanese, Korean, Russian, Simplified Chinese, Traditional Chinese and Spanish. More are welcome, and it doesn't require writing code: copy [`frontend/src/locales/en.json`](frontend/src/locales/en.json), translate the text after each `:`, open a pull request. Partial files are fine — anything missing falls back to English key by key.

Details, including placeholder and plural rules, in [docs/TRANSLATING.md](docs/TRANSLATING.md).

This is the language of the *app*. The language a card was printed in is a separate per-card field.

---

## License

[MIT](LICENSE).

---

## Support

If Scrybox saves you a ManaBox subscription, you can say thanks on Ko-fi:

[![Support me on Ko-fi](https://img.shields.io/badge/Ko--fi-Buy%20me%20a%20coffee-ff5e5b?logo=ko-fi&logoColor=white)](https://ko-fi.com/M4M31KYZ9Y)
