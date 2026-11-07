# photo-clearout

A small tool for clearing out old photos in [Immich](https://immich.app). It shows your library **oldest first**, as a square grid that fills the window. You **mark** photos as you go, then trash them page by page or all at once at the end. Everything trashed in a session can be **undone** until you press Finish.

It runs on your Mac and opens in your browser. Your photos stay in Immich: the tool only asks Immich for thumbnails and tells it which photos to move to its trash.

- Grid from **1 × 1** (one big photo) up to **12 × 12** (144 photos). The first photo of each day shows its date and place, with a green edge
- **Nothing is deleted while you browse.** Marks are saved as you go
- Trashed photos go to **Immich's trash** (restorable for 30 days), never deleted for good
- The API key always travels **encrypted**, through Tailscale
- Remembers where you stopped. Runs only while you use it

How it works inside, and how it keeps the key and your photos safe: [TECHNICAL.md](TECHNICAL.md).

---

## What you need

- A **Mac**, with **Rust** ([rustup.rs](https://rustup.rs)) to build the tool. The first run builds it, which takes a few minutes, once.
- **Tailscale** on the Mac and on Immich's server.
- **1Password**, with its command-line tool.

## Setup (once)

1. **Reach Immich through Tailscale.** Install Tailscale on the machine (or container) that runs Immich, and use its Tailscale address (`100.x.x.x`), not its home-network address (`192.168.x.x`). On home WiFi the Mac goes to a home-network address directly, unencrypted.
2. **Create an API key** in Immich, opened through its Tailscale address: **Account Settings → API Keys → New API Key**, with `asset.read`, `asset.view` and `asset.delete`.
3. **Store the key in 1Password:**
   - `brew install 1password-cli`
   - In the 1Password app, turn on **Settings → Developer → Integrate with 1Password CLI**.
   - On the key's field, choose **▾ → Copy Secret Reference**.
4. **Create `.env`** in this folder (it's never committed to git):
   ```
   IMMICH_URL=http://100.x.x.x:2283
   IMMICH_API_KEY_REF=op://<vault>/<item>/<field>
   ```

## Running it

Connect Tailscale on the Mac, then:

```bash
cd ~/Projects/photo-clearout
./run.sh
```

1Password asks for Touch ID. The tool checks that the connection really goes through Tailscale, that it can reach Immich, and that the key has the permissions it needs. Then it opens `http://127.0.0.1:3000/` in your browser. `./run.sh --help` lists the options.

### A session

1. Browse oldest first. **Mark** photos you don't want with `Space`, or click the circle on a photo. Marked photos are dimmed with a red ✕.
2. Move on with `n`. Marks stay, also if you stop and come back later.
3. Optional: **Trash marked on page** (`t`) trashes this page's marked photos now, with **Undo** (`u`) straight after.
4. When you're done, **End session** (`e`) shows every marked photo. Click any you want to keep (it turns green), then confirm.
5. The summary shows everything trashed this session. Click a photo to restore it, or **Restore all**. The **"N trashed"** counter opens it any time.
6. **Finish and stop** ends it. The photos stay restorable from Immich's own Trash page for 30 days.

### Keyboard shortcuts

| Key | Action |
|---|---|
| `←` `→` `↑` `↓` | Move between photos |
| `Space` or `d` | Mark / unmark |
| `a` | Mark / unmark all on this page |
| `Enter` or `f` | View full size (`←` `→` to flip, `Space` to mark) |
| `n` / `p` (or `PageDown` / `PageUp`) | Next / previous page |
| `t` | Trash the marked photos on this page |
| `u` | Undo the last trash |
| `e` | End session: review all marked, trash them |
| `+` (or `=`) / `-` | Bigger / smaller grid |
| `Esc` | Close full view / go back |
| `?` | Show shortcuts |

### Stopping

The tool stops when you press **Finish**, close the tab (within about 5 seconds), close the terminal, or press **Ctrl+C**. It never runs in the background. If a trash or restore is running, it finishes first. Your marks and undo list are kept for next time, even after a crash.

**Start over** (top right) goes back to the oldest photo and keeps your marks. Deleting the `.state/` folder forgets everything: where you stopped, the grid size and all marks. Nothing is deleted from Immich.

To remove the tool: delete this folder, then delete the API key in Immich and its item in 1Password.

---

## Troubleshooting

| Message | What to do |
|---|---|
| `traffic to 100.x.x.x would go out through en0, not through Tailscale` | Tailscale isn't connected on the Mac: menu bar icon → **Connect** |
| `IMMICH_URL is plain http:// to …, which is not a Tailscale address` | Put Immich's Tailscale address (`http://100.x.x.x:2283`) in `.env` |
| `… resolves to …, which is not a Tailscale address` | Connect Tailscale, or use Immich's `100.x` address instead of its name |
| `Could not read the API key from 1Password` | Unlock 1Password, and check **Integrate with 1Password CLI** is on and `IMMICH_API_KEY_REF` is the exact secret reference |
| `cannot reach Immich` | Is Tailscale connected on the Mac? Is Immich's server running, with Tailscale running on it (`tailscale status` there)? |
| `HTTP 401` | Wrong or deleted key: check 1Password has the key listed in Immich |
| `HTTP 403` or `the API key may not …` | The key is missing a permission (see Setup step 2) |
| `Immich answered with a redirect` | `IMMICH_URL` isn't Immich's own address |
| `could not listen on 127.0.0.1:3000` | It's already running in another terminal |
| `Rust is not installed` | Install it from [rustup.rs](https://rustup.rs) |

## Known limits

- Tested against a pretend Immich. With the real one it has connected, listed photos and marked them. **Trashing and undo haven't been tried against it yet.**
- Mac only.
