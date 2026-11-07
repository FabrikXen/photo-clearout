# photo-clearout: how it works

How the tool works inside, how it keeps the API key and your photos safe, and what has changed over time. For setting it up and using it, see [README.md](README.md).

---

## The pieces

```
Browser (the grid page)  ⇄  photo-clearout (Rust, on your Mac)  ⇄  Immich (your server)
   127.0.0.1:3000              holds the API key                    stores the photos
                                                     Tailscale tunnel
```

The browser never talks to Immich and never sees the API key. It asks the Rust server, and the Rust server asks Immich.

### Code layout

| File | Job |
|---|---|
| `run.sh` | Builds the tool, reads `.env`, fetches the key from 1Password, starts the tool |
| `src/main.rs` | Start-up: read settings, the Tailscale check, check Immich and the key's permissions, repair an unfinished trash, start the server on 127.0.0.1 |
| `src/config.rs` | Reads and checks the settings, including which addresses may use plain `http://` |
| `src/tunnel.rs` | Checks plain `http://` to Immich really goes through Tailscale's tunnel, and finds this Mac's Tailscale address |
| `src/immich.rs` | **Everything that talks to Immich.** The rest of the code only sees the `PhotoSource` trait |
| `src/paging.rs` | Works out which photos belong on each page |
| `src/progress.rs` | Saves and loads the `.state/` files |
| `src/web.rs` | The local web server: the page, the JSON API, the security checks, and trashing / restoring |
| `static/` | The page: `index.html`, `app.css`, `app.js` (built into the program at compile time) |
| `tests/` | Tests against a pretend Immich and a fake photo library |

---

## Keeping the API key safe

The key can trash every photo in Immich, so it's handled carefully all the way through.

**Where it lives.** Only in 1Password. `.env` holds a *secret reference* (`op://vault/item/field`), which says where the key is but isn't secret itself. `run.sh` fetches the key with `op read` each time it starts, after Touch ID. It never appears in a file, in the code or in your shell history.

**Build first, key second.** Building the tool runs code from the Rust libraries it uses. So `run.sh` builds first, and only then fetches the key and starts the finished program. An `IMMICH_API_KEY` already set in your environment is hidden from the build. The browser started with `open` doesn't get the key either.

**In the program.** The key is only sent to Immich. It never goes to the browser, the request header is marked sensitive, `Config`'s debug output prints `<redacted>`, and error messages strip URLs. The tool **never follows redirects**, so a redirect can't carry the key to another server.

### Encrypted on the way to Immich

The key is sent with every request, so it must never cross WiFi in the clear.

**Allowed addresses.** `IMMICH_URL` may be:
- plain `http://` to a Tailscale address (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`, or a `….ts.net` name)
- plain `http://` to this Mac itself (`localhost`, `127.0.0.1`, e.g. through an SSH tunnel)
- any `https://` address

Anything else, such as a home-network address like `192.168.1.10`, is refused at start-up. On home WiFi the Mac would go there directly, unencrypted (`route get <address>` would show `en0`, the WiFi).

**Checking the real route.** A Tailscale-looking address isn't enough on its own: with Tailscale off, traffic to `100.x.x.x` goes out over WiFi in the clear. So with plain `http://` to another machine, before anything is sent, `src/tunnel.rs`:

1. looks up the host, and requires a Tailscale address (a spoofed lookup over WiFi can't point it elsewhere)
2. asks macOS which interface it would use (`/sbin/route -n get`), and requires one that carries a Tailscale address of this Mac's own. Other VPNs also use `utun` interfaces, so the name alone isn't trusted

**Pinning the connection.** Every connection then goes to the checked address, from this Mac's Tailscale address. If Tailscale disconnects mid-session, that address disappears, and requests to Immich fail instead of falling back to WiFi.

Best is to run Tailscale on the machine or container that runs Immich itself, so the tunnel ends at Immich and there's no unencrypted hop anywhere.

### The key's permissions

The key needs `asset.read` (search, checking a photo), `asset.view` (thumbnails and previews) and `asset.delete` (trash). Immich has no separate trash or restore permission, so undo is expected to work with these three.

At start-up the tool checks the key may list photos, trash and restore. The trash and restore checks send an **empty** list to the endpoints for specific photos (never "restore all" or "empty trash"), with `force: false`, so nothing can change. Immich checks permissions before it looks at the list, so a missing permission shows as 401/403, and the tool names it.

---

## The local server

It listens on **127.0.0.1 only**, so nothing else on the network can connect. Websites you have open in the browser could still *try* to send it requests, so:

- The `Host` header must be exactly `127.0.0.1:PORT` or `localhost:PORT`, which blocks DNS-rebinding tricks.
- Any `Origin` must be the tool itself (`null` is refused).
- `Sec-Fetch-Site` must be `same-origin` or `none`, so other sites can't even embed its images.
- Every change needs a 256-bit **token** that's new each run and only in the tool's own page. It's compared in constant time. The goodbye sent as the tab closes carries it in its body, because the browser can't add headers at that moment.
- Responses carry a strict Content-Security-Policy (no inline script), `Cross-Origin-Resource-Policy: same-origin`, `nosniff`, `no-referrer`, `X-Frame-Options: DENY`, and `Cache-Control: no-store` for photos, so the browser keeps no copies on disk.
- The page is built with `textContent`, never `innerHTML`, so a file name like `<script>` is shown as text.
- Only JPEG, WebP, PNG, AVIF and HEIC/HEIF images from Immich are passed on, up to 20 MB. Not, say, SVG, which can contain scripts.
- Every photo id must be a valid UUID before it goes near a URL: ids from the page, from the state files, and again inside the Immich client. That stops something like `../users/me` from calling a different Immich API.

### The local API

| Request | Does |
|---|---|
| `GET /` | The page |
| `POST /api/page` `{size, cursor}` | One page of photos. `cursor: null` = where you stopped last time |
| `POST /api/marks` `{add, remove}` | Mark / unmark photos (saved to `.state/marked.json`). Empty = list marks |
| `POST /api/trash-marked` `{ids}` | Trash these **marked** photos (e.g. this page's) |
| `POST /api/end-session` `{ids, keep}` | Trash `ids` (all **marked**), unmark `keep` (kept on the review screen). Other marks are left alone |
| `POST /api/restore` `{ids}` | Undo: restore photos trashed this session |
| `POST /api/finish` | Clear the undo list and stop the tool |
| `POST /api/ping` | Heartbeat: "the page is still open" |
| `POST /api/bye` `{token}` | Sent as the tab closes |
| `POST /api/reset` | Start again from the oldest |
| `POST /api/settings` `{cols}` | Save the grid size (1–12) |
| `GET /api/image/{id}?size=thumbnail\|preview` | A thumbnail or larger preview, fetched from Immich |

Trash, restore and end-session replies include `changed` (how many were trashed or restored) and `gone` (how many turned out to no longer exist in Immich).

---

## Safety rules for your photos

- **Nothing is deleted while browsing.** Photos are only *marked*.
- **Only marked photos can be trashed.** A request containing any unmarked photo is refused whole, and nothing is trashed.
- **Only photos trashed this session can be restored** through the tool. Older ones are on Immich's own Trash page.
- **Never permanent.** The tool only ever sends `force: false` (move to trash), and a test checks it.

## Not failing halfway

- **Start-up checks** (Tailscale route, Immich reachable, key permissions) happen before you start, so you don't find out mid-session.
- **Marks are saved to disk as you go**, so a crash or Ctrl+C loses nothing.
- **Trashing goes in batches of up to 500.** If a batch fails, the message says exactly how many made it. Those are unmarked and on the undo list, and the rest stay marked.
- **A timeout isn't taken as "nothing happened".** Immich may have trashed the batch anyway. The tool asks Immich about each photo, so the ones that did go stay on the undo list. If Immich can't be asked either, they're left "in flight" and checked at the next start.
- **Write-ahead undo list.** A photo goes onto the undo list *before* Immich is asked to trash it. If the tool dies in between, the photo is both marked and on the undo list. At the next start, the tool asks Immich about exactly those photos and fixes the records. A photo Immich can't answer about is left for next time, rather than stopping the tool from starting.
- **Photos deleted for good in Immich** (in Immich itself, or purged from its trash after 30 days) don't block anything. Immich refuses a whole batch (HTTP 400) if one photo in it doesn't exist, so the tool asks about each photo, takes the missing ones off its lists, and retries the rest. The page says how many were taken off.
- **One thing at a time.** Marking, trashing, restoring, ending the session and crash repair share one lock, so they never overlap, even from two tabs.
- **Jobs finish even if the browser goes away.** Trash and restore run as background tasks, and the tool never exits while one is running. A job that crashes is reported, and can't stop the tool from shutting down.
- **Saved state** is written atomically and flushed to disk: a new file, then a rename. A damaged file is moved aside (`….json.corrupt`). A file that exists but can't be read is **never** overwritten, so a read error can't wipe your marks or undo list.

### Saved state

| File | Holds |
|---|---|
| `.state/progress.json` | where you stopped (the cursor of the last page shown) |
| `.state/settings.json` | your grid size |
| `.state/marked.json` | photos marked, not yet trashed |
| `.state/trashed.json` | trashed this session, for undo |

`.state/` is 0700 and its files 0600, readable only by you, because they hold file names and places.

### Why pages use dates, not page numbers

If pages were numbered, trashing 5 photos on page 1 would slide the first 5 photos of page 2 back onto page 1, and "next page" would skip them.

Instead, each page starts from a **cursor**: "photos taken after this moment". Trashing photos doesn't move a moment in time, so nothing is skipped. Burst shots can share the exact same timestamp, so the cursor also remembers which of those were already shown. If a burst is bigger than one request to Immich, the tool asks again for as many as Immich allows (1,000) rather than stopping early. Beyond 1,000 photos at one timestamp it reports an error instead of skipping any.

### Running only while you use it

- It refuses to start unless it's in the foreground of a terminal (not `&`, `nohup` or a service), and stops if moved to the background.
- It stops when you press Finish, close the terminal, or press Ctrl+C. Any running trash or restore finishes first.
- Closing the tab sends a goodbye, and the tool stops after 5 s unless a page gets back in touch (a reload does).
- Backstop: the page sends a heartbeat every 10 s, and the tool stops after 2 minutes without one. It uses a clock that a change to the Mac's time can't affect.

---

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `IMMICH_URL` | *(required)* | Immich's address. Read from `.env` by `run.sh` |
| `IMMICH_API_KEY_REF` | *(required by `run.sh`)* | Where the key is in 1Password. Read from `.env` |
| `IMMICH_API_KEY` | *(required by the tool)* | The key itself. `run.sh` fetches it from 1Password; if it's already set, it's used as is |
| `PHOTO_CLEAROUT_PORT` | `3000` | Local port for the page (1024–65535) |
| `PHOTO_CLEAROUT_STATE_DIR` | `.state` | Folder for the saved state, relative to where the tool starts (`run.sh` starts it in the project folder) |
| `PHOTO_CLEAROUT_NO_BROWSER` | *(unset)* | `1`, `yes` or `true` = don't open the browser. Unset, empty, `0`, `false` or `no` = open it |

`run.sh` reads only these lines from `.env` and never runs the file. Quotes around a value are fine. A variable already set in your environment takes priority.

Outside this folder the tool only uses 1Password (the key) and `~/.cargo/registry` (Rust's shared download cache). Nothing goes into the browser's storage: the grid size is saved by the server.

---

## Tests

```bash
cargo test
```

97 tests, none of them using real photos or your real Immich:

- **Unit tests** (inside `src/`): settings (which addresses count as encrypted and need the tunnel check), reading macOS's route and interface information, paging (bursts, deletions, last page), saving state (never over an unreadable file, private files).
- **`tests/immich_client.rs`**: the real Immich client against a pretend Immich (`wiremock`). The exact requests (key header, oldest first, `force: false`, empty lists for the permission check), no redirects followed, only photo types accepted, Immich's 400 for a missing photo, error handling.
- **`tests/web.rs`**: the web server against a fake photo library that behaves like Immich. Paging (including a burst bigger than one request), resuming, marking, trashing (per page, end session, batches, partial failure, a timeout after Immich already trashed, a photo deleted for good), undo (including a photo purged from Immich's trash), Finish, closing the browser mid-trash, marking while a trash runs, crash repair, a job that crashes, idle stop, and the security rules.

In Claude Code's sandbox, `tests/immich_client.rs` can't open its local port ("Operation not permitted") unless `sandbox.network.allowLocalBinding` is `true`. The other tests don't need it.

## Known limits

- Built against Immich's documented v2 API and tested against a pretend Immich. With the real Immich it has connected, listed photos and marked them. Trashing, undo, the permission check and the Tailscale route check haven't been tried against it yet. Immich answering 400 (not 404) for a missing photo comes from Immich's code, and hasn't been seen with the real one yet.
- macOS only: the terminal checks, `open`, `/sbin/route` and the 1Password CLI.

---

## History

**2026-10-07: built.** First version, followed by a review of security, ease of use and code quality. First run against a real Immich: it connected, listed photos and marked them.

**2026-11-04: encrypted connection and 1Password.**
- Immich is now reached through its Tailscale address instead of its home-network address, so the key no longer crosses WiFi unencrypted.
- The key now lives only in 1Password. `run.sh` reads it with `op read`, using `IMMICH_API_KEY_REF` from `.env`. The Keychain is no longer used.

**2026-11-07: review fixes.**
- **Security:** plain `http://` refused unless on Tailscale; redirects never followed; ids checked everywhere, including from the state files; only photo image types, up to 20 MB; cross-site requests and embedding refused; `.state/` private.
- **Data safety:** a read error can't wipe saved state; a timeout no longer drops trashed photos from the undo list; one lock for everything that changes marks or the undo list; end-session only unmarks the photos it reviewed; a crashed job can't block shutdown; big bursts no longer end paging early; files flushed to disk.
- **Start-up and running:** the key's permissions are checked at start-up; first-build message, and a clear error if Rust is missing; `--help` and `--version`; quoted `.env` values; a clearer `PHOTO_CLEAROUT_NO_BROWSER`.

**2026-11-07: second review.** Fixed from it:
- the Tailscale route check and connection pinning (`src/tunnel.rs`)
- handling of photos no longer in Immich
- building before the key is fetched

The README was split: using the tool stays in README.md, and how it works moved here.
