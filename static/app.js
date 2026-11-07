"use strict";

// The page talks only to the local Rust server, never to Immich directly.
// Every POST carries the secret token from the page's <meta> tag.
//
// Flow: you MARK photos while browsing (saved by the server as you go).
// Photos are trashed only when you ask: "Trash marked on page", or End
// session (review → confirm). Everything trashed this session can be undone
// from the summary screen until you press Finish.

(() => {
  const token = document.querySelector('meta[name="clearout-token"]').content;
  const $ = (id) => document.getElementById(id);

  const grid = $("grid");
  const statusEl = $("status");
  const statusText = $("status-text");
  const statusAction = $("status-action");
  const positionEl = $("position");
  const daysEl = $("days");
  const sizeSel = $("size");
  const prevBtn = $("prev");
  const nextBtn = $("next");
  const markedEl = $("marked-count");
  const trashPageBtn = $("trash-page");
  const trashedBtn = $("trashed-count");
  const endBtn = $("end");
  const resetBtn = $("reset");
  const viewer = $("viewer");
  const viewerImg = $("viewer-img");
  const viewerCaption = $("viewer-caption");
  const viewerMark = $("viewer-mark");
  const review = $("review");
  const reviewTitle = $("review-title");
  const reviewGrid = $("review-grid");
  const reviewStatus = $("review-status");
  const reviewConfirm = $("review-confirm");
  const summary = $("summary");
  const summaryTitle = $("summary-title");
  const summaryGrid = $("summary-grid");
  const summaryStatus = $("summary-status");
  const summaryRestoreAll = $("summary-restore-all");
  const summaryFinish = $("summary-finish");
  const ended = $("ended");
  const endedMsg = $("ended-msg");
  const stoppedBanner = $("stopped");
  const help = $("help");

  // The grid is cols × cols photos.
  const MIN_COLS = 1;
  const MAX_COLS = 12;
  const DEFAULT_COLS = 5;
  // From this many columns on, tiles are small: hide captions.
  const DENSE_FROM = 7;
  // Up to this many columns, tiles are big: use the sharper preview image.
  const PREVIEW_UP_TO = 3;
  // Tells the server this page is still open. Without it, the server stops
  // itself shortly after the browser is closed.
  const HEARTBEAT_MS = 10_000;

  const state = {
    cols: loadCols(),
    cursor: null,        // where the current page starts
    next: null,          // where the next page starts (null = newest reached)
    history: [],         // cursors of earlier pages, for "previous"
    items: [],           // photos on this page
    marked: new Map(),   // id → photo, across all pages
    trashed: [],         // trashed this session (restorable)
    lastTrashed: [],     // ids of the most recent trash, for "u" / Undo
    focus: 0,
    viewing: -1,         // index shown full size, or -1
    busy: false,
    screen: "grid",      // "grid" | "review" | "summary" | "ended"
    keep: new Set(),     // ids kept on the review screen
    stopped: false,
  };

  // Photos store the local wall-clock time as if it were UTC, so format in UTC
  // to show the time as it was where the photo was taken.
  const dayFmt = new Intl.DateTimeFormat(undefined, {
    weekday: "short", day: "numeric", month: "long", year: "numeric", timeZone: "UTC",
  });
  const shortDayFmt = new Intl.DateTimeFormat(undefined, {
    day: "numeric", month: "short", year: "2-digit", timeZone: "UTC",
  });
  const monthFmt = new Intl.DateTimeFormat(undefined, { month: "short", year: "numeric", timeZone: "UTC" });
  const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", timeZone: "UTC" });

  const when = (a) => new Date(a.local_date_time || a.taken_at);
  const dayKey = (a) => (a.local_date_time || a.taken_at).slice(0, 10);
  const pageSize = () => state.cols * state.cols;
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  const byDate = (a, b) => (a.taken_at < b.taken_at ? -1 : a.taken_at > b.taken_at ? 1 : 0);
  // Photos deleted for good in Immich meanwhile are dropped from the lists.
  const goneNote = (n) => (n ? ` ${plural(n, "photo")} no longer in Immich (deleted there for good) ${n === 1 ? "was" : "were"} taken off the list.` : "");

  // The grid size is saved by the server in the project's .state/ folder,
  // not in the browser, so nothing about the tool is stored outside it.
  function loadCols() {
    const n = Number(document.querySelector('meta[name="clearout-cols"]').content);
    return Number.isInteger(n) && n >= MIN_COLS && n <= MAX_COLS ? n : DEFAULT_COLS;
  }
  function saveCols(n) {
    api("/api/settings", { cols: n }).catch((e) => setStatus(`Could not save grid size: ${e.message}`, "error"));
  }

  async function api(path, body) {
    let res;
    try {
      res = await fetch(path, {
        method: "POST",
        headers: { "content-type": "application/json", "x-clearout-token": token },
        body: JSON.stringify(body),
      });
    } catch (e) {
      // No answer at all: the tool isn't running any more.
      showStopped();
      throw new Error("the tool is not running");
    }
    let data = null;
    try { data = await res.json(); } catch {}
    if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
    return data;
  }

  function showStopped() {
    if (state.screen === "ended") return;
    state.stopped = true;
    stoppedBanner.hidden = false;
  }

  /// action = optional { label, run } shown as a button after the message.
  function setStatus(msg, kind = "", action = null) {
    statusText.textContent = msg;
    statusEl.className = kind;
    statusAction.hidden = !action;
    statusAction.onclick = null;
    if (action) {
      statusAction.textContent = action.label;
      statusAction.onclick = (ev) => { ev.currentTarget.blur(); action.run(); };
    }
  }

  // Every marking/trashing call returns the server's real state.
  function applySession(data) {
    state.marked = new Map(data.marked.map((a) => [a.id, a]));
    state.trashed = data.trashed;
    afterMarkChange();
  }

  // ---- Loading pages ----

  async function load(cursor) {
    if (state.busy) return;
    state.busy = true;
    setStatus("Loading…");
    try {
      const data = await api("/api/page", { size: pageSize(), cursor });
      state.cursor = data.cursor;
      state.next = data.next;
      state.items = data.items;
      state.focus = Math.min(state.focus, Math.max(0, state.items.length - 1));
      render();
      if (!state.items.length) setStatus("No more photos: you've reached the newest. 🎉", "ok");
      else if (!state.next) setStatus("Last page: these are your newest photos.");
      else setStatus("");
    } catch (e) {
      setStatus(`Could not load photos: ${e.message}`, "error");
    } finally {
      state.busy = false;
    }
  }

  // Built with createElement/textContent, never innerHTML, so a file name
  // like "<script>…" is shown as text and can't run.
  function render() {
    grid.replaceChildren();
    grid.style.setProperty("--cols", String(state.cols));
    grid.classList.toggle("dense", state.cols >= DENSE_FROM);

    // The line above the grid: every day on this page, with its places.
    const days = [];
    for (const a of state.items) {
      const key = dayKey(a);
      let day = days.find((d) => d.key === key);
      if (!day) days.push((day = { key, date: when(a), places: new Set() }));
      if (a.place) day.places.add(a.place);
    }
    daysEl.textContent = days
      .map((d) => [dayFmt.format(d.date), ...d.places].join(" · "))
      .join("   |   ");

    // Mark the first photo of each day with its date.
    let previousDay = null;
    state.items.forEach((a, i) => {
      const day = dayKey(a);
      grid.append(makeTile(a, i, day !== previousDay ? days.find((d) => d.key === day) : null));
      previousDay = day;
    });

    layout();
    updateToolbar();
    updateFocus();
  }

  // Size the grid so `cols` rows of square tiles fit the window. A square
  // grid is as tall as it is wide, so its width is capped by the height left
  // below the toolbar.
  function layout() {
    const heightLeft = window.innerHeight - grid.getBoundingClientRect().top - 16;
    const widthLeft = grid.parentElement.clientWidth;
    grid.style.width = `${Math.max(120, Math.min(widthLeft, heightLeft))}px`;
  }

  function makeTile(a, i, newDay) {
    const t = document.createElement("div");
    t.className = "tile";
    t.dataset.index = String(i);
    if (state.marked.has(a.id)) t.classList.add("marked");

    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = a.file_name;
    const quality = state.cols <= PREVIEW_UP_TO ? "preview" : "thumbnail";
    img.src = `/api/image/${encodeURIComponent(a.id)}?size=${quality}`;
    img.addEventListener("click", () => { state.focus = i; openViewer(i); });

    const check = document.createElement("button");
    check.className = "check";
    check.title = "Mark for the trash (Space)";
    check.setAttribute("aria-label", `Mark ${a.file_name}`);
    check.addEventListener("click", (ev) => {
      ev.stopPropagation();
      ev.currentTarget.blur();
      state.focus = i;
      toggleMark(i);
      updateFocus();
    });

    const cap = document.createElement("span");
    cap.className = "cap";
    cap.textContent = timeFmt.format(when(a)) + (a.is_video ? " · video" : "");

    t.append(img, check, cap);

    if (newDay) {
      t.classList.add("new-day");
      const label = document.createElement("span");
      label.className = "day";
      const dense = state.cols >= DENSE_FROM;
      label.textContent = dense
        ? shortDayFmt.format(newDay.date)
        : [dayFmt.format(newDay.date), ...newDay.places].join(" · ");
      t.append(label);
    }
    return t;
  }

  const tileEls = () => [...grid.querySelectorAll(".tile")];
  const markedOnPage = () => state.items.filter((a) => state.marked.has(a.id));

  function refreshTile(i) {
    const a = state.items[i];
    const el = tileEls()[i];
    if (a && el) el.classList.toggle("marked", state.marked.has(a.id));
  }

  function updateToolbar() {
    const n = state.marked.size;
    markedEl.textContent = `${n} marked`;
    markedEl.classList.toggle("some", n > 0);
    const onPage = markedOnPage().length;
    trashPageBtn.disabled = onPage === 0;
    trashPageBtn.textContent = onPage ? `Trash ${onPage} on page` : "Trash marked on page";
    trashedBtn.textContent = `${state.trashed.length} trashed`;
    prevBtn.disabled = state.history.length === 0;
    nextBtn.disabled = !state.next;
    if (state.items.length) {
      const first = monthFmt.format(when(state.items[0]));
      const last = monthFmt.format(when(state.items[state.items.length - 1]));
      positionEl.textContent = first === last ? first : `${first} – ${last}`;
    } else {
      positionEl.textContent = "";
    }
  }

  function updateFocus() {
    tileEls().forEach((el, i) => el.classList.toggle("focused", i === state.focus));
    const el = tileEls()[state.focus];
    if (el && state.viewing < 0) el.scrollIntoView({ block: "nearest" });
  }

  function setFocus(i) {
    if (!state.items.length) return;
    state.focus = Math.max(0, Math.min(state.items.length - 1, i));
    updateFocus();
  }

  // Up/down: one row = `cols` photos. Stay put at the top/bottom edge.
  function vertical(dir) {
    const target = state.focus + dir * state.cols;
    return target >= 0 && target < state.items.length ? target : state.focus;
  }

  // ---- Marking ----

  // Show the change straight away, save it in the background, and undo it
  // on screen if saving fails, so the screen never shows a mark that isn't saved.
  async function changeMarks(add, remove) {
    add.forEach((a) => state.marked.set(a.id, a));
    remove.forEach((a) => state.marked.delete(a.id));
    afterMarkChange();
    try {
      await api("/api/marks", { add, remove: remove.map((a) => a.id) });
    } catch (e) {
      add.forEach((a) => state.marked.delete(a.id));
      remove.forEach((a) => state.marked.set(a.id, a));
      afterMarkChange();
      setStatus(`Could not save mark: ${e.message}`, "error");
    }
  }

  function afterMarkChange() {
    state.items.forEach((_, i) => refreshTile(i));
    if (state.viewing >= 0) showViewer();
    updateToolbar();
  }

  function toggleMark(i) {
    const a = state.items[i];
    if (!a) return;
    if (state.marked.has(a.id)) changeMarks([], [a]);
    else changeMarks([a], []);
  }

  function toggleMarkAll() {
    const unmarked = state.items.filter((a) => !state.marked.has(a.id));
    if (unmarked.length) changeMarks(unmarked, []);
    else changeMarks([], state.items);
  }

  // ---- Trash this page, and undo ----

  async function trashPage() {
    const ids = markedOnPage().map((a) => a.id);
    if (!ids.length || state.busy) return;
    if (!confirm(`Move ${plural(ids.length, "marked photo")} on this page to Immich's trash?\n\nYou can undo this.`)) return;

    state.busy = true;
    setStatus(`Moving ${plural(ids.length, "photo")} to the trash…`);
    let done = null;
    try {
      done = await api("/api/trash-marked", { ids });
      applySession(done);
      state.lastTrashed = ids;
    } catch (e) {
      setStatus(e.message, "error");
      try { applySession(await api("/api/marks", {})); } catch {}
    } finally {
      state.busy = false;
    }
    closeViewer();
    await load(state.cursor); // the trashed photos disappear, later ones move up
    if (done) {
      setStatus(`Moved ${plural(done.changed, "photo")} to Immich's trash.${goneNote(done.gone)}`, "ok", { label: "Undo (u)", run: undoLast });
    }
  }

  async function restore(ids) {
    const data = await api("/api/restore", { ids });
    applySession(data);
    return data;
  }

  async function undoLast() {
    const ids = state.lastTrashed.filter((id) => state.trashed.some((a) => a.id === id));
    if (!ids.length || state.busy) return;
    state.busy = true;
    setStatus("Restoring…");
    try {
      const done = await restore(ids);
      state.lastTrashed = [];
      state.busy = false;
      await load(state.cursor);
      setStatus(`Restored ${plural(done.changed, "photo")} from the trash.${goneNote(done.gone)}`, "ok");
    } catch (e) {
      state.busy = false;
      setStatus(`Undo failed: ${e.message}`, "error");
    }
  }

  // ---- Full-size viewer ----

  function openViewer(i) {
    if (!state.items[i]) return;
    state.viewing = i;
    state.focus = i;
    showViewer();
  }

  function showViewer() {
    const a = state.items[state.viewing];
    if (!a) return closeViewer();
    viewer.hidden = false;
    viewerImg.src = `/api/image/${encodeURIComponent(a.id)}?size=preview`;
    viewerImg.alt = a.file_name;
    const d = when(a);
    viewerCaption.textContent = [
      `${dayFmt.format(d)} ${timeFmt.format(d)}`,
      a.place,
      a.file_name,
      `${state.viewing + 1} / ${state.items.length}`,
    ].filter(Boolean).join(" · ");
    const marked = state.marked.has(a.id);
    viewer.classList.toggle("marked", marked);
    viewerMark.textContent = marked ? "Marked ✕ (Space to unmark)" : "Mark (Space)";
  }

  function closeViewer() {
    if (state.viewing < 0 && viewer.hidden) return;
    viewer.hidden = true;
    viewerImg.removeAttribute("src");
    state.viewing = -1;
    updateFocus();
  }

  // ---- Paging ----

  function goNext() {
    if (!state.next || state.busy) return;
    state.history.push(state.cursor);
    state.focus = 0;
    load(state.next);
    window.scrollTo(0, 0);
  }

  function goPrev() {
    if (!state.history.length || state.busy) return;
    state.focus = 0;
    load(state.history.pop());
    window.scrollTo(0, 0);
  }

  function changeCols(step) {
    const next = Math.max(MIN_COLS, Math.min(MAX_COLS, state.cols + step));
    if (next !== state.cols) setCols(next);
  }

  function setCols(n) {
    state.cols = n;
    sizeSel.value = String(n);
    saveCols(n);
    load(state.cursor);
  }

  async function startOver() {
    if (state.busy) return;
    if (!confirm("Start again from the oldest photo?\n\nYour marks are kept.")) return;
    try {
      await api("/api/reset", {});
    } catch (e) {
      setStatus(`Could not reset: ${e.message}`, "error");
      return;
    }
    state.history = [];
    state.focus = 0;
    load(null);
  }

  // ---- Screens ----

  function showScreen(name) {
    state.screen = name;
    review.hidden = name !== "review";
    summary.hidden = name !== "summary";
    ended.hidden = name !== "ended";
  }

  async function backToGrid() {
    showScreen("grid");
    await load(state.cursor); // restored photos may have come back
  }

  function photoTile(a, label, kept, onClick) {
    const t = document.createElement("div");
    t.className = "review-tile";
    t.classList.toggle("kept", kept);
    const img = document.createElement("img");
    img.loading = "lazy";
    img.alt = a.file_name;
    img.src = `/api/image/${encodeURIComponent(a.id)}?size=thumbnail`;
    const badge = document.createElement("span");
    badge.className = "state";
    badge.textContent = label;
    const cap = document.createElement("span");
    cap.className = "cap";
    cap.textContent = shortDayFmt.format(when(a));
    t.append(img, badge, cap);
    t.addEventListener("click", () => onClick(t));
    return t;
  }

  // ---- End session, step 1: review ----

  async function openReview() {
    if (state.busy || state.stopped) return;
    closeViewer();
    try {
      // Reload from the server so the review shows exactly what's saved.
      applySession(await api("/api/marks", {}));
    } catch (e) {
      setStatus(`Could not load marks: ${e.message}`, "error");
      return;
    }
    state.keep = new Set();
    reviewStatus.textContent = "";
    reviewStatus.className = "screen-status";
    showScreen("review");
    renderReview();
  }

  function renderReview() {
    const list = [...state.marked.values()].sort(byDate);
    reviewGrid.replaceChildren(...list.map((a) => {
      const kept = state.keep.has(a.id);
      return photoTile(a, kept ? "Keep" : "Trash", kept, () => {
        if (state.keep.has(a.id)) state.keep.delete(a.id);
        else state.keep.add(a.id);
        renderReview();
      });
    }));
    const toTrash = list.length - state.keep.size;
    reviewTitle.textContent = list.length ? `Review: ${plural(list.length, "marked photo")}` : "Nothing is marked";
    reviewConfirm.textContent = toTrash ? `Trash ${toTrash}` : "Done (trash nothing)";
  }

  async function confirmEnd() {
    if (state.busy) return;
    const ids = [...state.marked.keys()].filter((id) => !state.keep.has(id));
    // Only the photos this review showed are unmarked; any marked meanwhile stay.
    const keep = [...state.marked.keys()].filter((id) => state.keep.has(id));
    if (ids.length && !confirm(`Move ${plural(ids.length, "photo")} to Immich's trash?\n\nYou can undo this on the next screen.`)) return;

    state.busy = true;
    reviewConfirm.disabled = true;
    reviewStatus.className = "screen-status";
    reviewStatus.textContent = ids.length ? `Moving ${plural(ids.length, "photo")} to the trash…` : "";
    try {
      const data = await api("/api/end-session", { ids, keep });
      applySession(data);
      state.lastTrashed = ids;
      openSummary((data.changed ? `Moved ${plural(data.changed, "photo")} to Immich's trash.` : "Nothing was trashed.") + goneNote(data.gone));
    } catch (e) {
      // Show exactly what happened, then reload: photos that did make it to
      // the trash are no longer marked, the rest still are.
      reviewStatus.className = "screen-status error";
      reviewStatus.textContent = e.message;
      try { applySession(await api("/api/marks", {})); } catch {}
      state.keep = new Set([...state.keep].filter((id) => state.marked.has(id)));
      renderReview();
    } finally {
      state.busy = false;
      reviewConfirm.disabled = false;
    }
  }

  // ---- Summary: everything trashed this session, with undo ----

  function openSummary(message = "") {
    closeViewer();
    summaryStatus.className = "screen-status ok";
    summaryStatus.textContent = message;
    showScreen("summary");
    renderSummary();
  }

  function renderSummary() {
    const list = [...state.trashed].sort(byDate);
    summaryGrid.replaceChildren(...list.map((a) =>
      photoTile(a, "Trashed", false, async (tile) => {
        if (state.busy) return;
        tile.classList.add("busy");
        await restoreFromSummary([a.id]);
      })
    ));
    summaryTitle.textContent = list.length ? `Trashed this session: ${list.length}` : "Nothing trashed this session";
    summaryRestoreAll.disabled = list.length === 0;
  }

  async function restoreFromSummary(ids) {
    state.busy = true;
    summaryStatus.className = "screen-status";
    summaryStatus.textContent = "Restoring…";
    try {
      const done = await restore(ids);
      summaryStatus.className = "screen-status ok";
      summaryStatus.textContent = `Restored ${plural(done.changed, "photo")}.${goneNote(done.gone)}`;
    } catch (e) {
      summaryStatus.className = "screen-status error";
      summaryStatus.textContent = e.message;
      try { applySession(await api("/api/marks", {})); } catch {}
    } finally {
      state.busy = false;
      renderSummary();
    }
  }

  async function finish() {
    if (state.busy) return;
    const n = state.trashed.length;
    const question = n
      ? `Finish and stop the tool?\n\nThe ${plural(n, "trashed photo")} can still be restored from Immich's Trash page for 30 days, but no longer from here.`
      : "Finish and stop the tool?";
    if (!confirm(question)) return;
    try {
      await api("/api/finish", {});
    } catch (e) {
      summaryStatus.className = "screen-status error";
      summaryStatus.textContent = `Could not finish: ${e.message}`;
      return;
    }
    endedMsg.textContent = n ? `${plural(n, "photo")} moved to Immich's trash this session.` : "Nothing was trashed this session.";
    showScreen("ended");
    clearInterval(heartbeat);
  }

  // ---- Buttons ----

  const click = (el, fn) => el.addEventListener("click", (ev) => { ev.currentTarget.blur(); fn(); });
  click(prevBtn, goPrev);
  click(nextBtn, goNext);
  click(trashPageBtn, trashPage);
  click(trashedBtn, () => openSummary());
  click(endBtn, openReview);
  click(resetBtn, startOver);
  click($("help-open"), () => help.showModal());
  click($("help-close"), () => help.close());
  click($("viewer-close"), closeViewer);
  click(viewerMark, () => toggleMark(state.viewing));
  click($("review-back"), backToGrid);
  click(reviewConfirm, confirmEnd);
  click($("summary-back"), backToGrid);
  click(summaryRestoreAll, () => {
    if (state.busy || !state.trashed.length) return;
    if (confirm(`Restore all ${plural(state.trashed.length, "photo")} from the trash?`)) {
      restoreFromSummary(state.trashed.map((a) => a.id));
    }
  });
  click(summaryFinish, finish);
  sizeSel.addEventListener("change", () => { sizeSel.blur(); setCols(Number(sizeSel.value)); });
  window.addEventListener("resize", layout);

  // ---- Keyboard ----

  document.addEventListener("keydown", (ev) => {
    if (state.screen === "ended" || help.open || ev.metaKey || ev.ctrlKey || ev.altKey) return;
    if (ev.target === sizeSel) return;
    const k = ev.key;

    if (state.screen === "review" || state.screen === "summary") {
      if (k === "Escape") { backToGrid(); ev.preventDefault(); }
      return;
    }

    if (state.viewing >= 0) {
      if (k === "Escape") closeViewer();
      else if (k === "ArrowRight") openViewer(Math.min(state.items.length - 1, state.viewing + 1));
      else if (k === "ArrowLeft") openViewer(Math.max(0, state.viewing - 1));
      else if (k === " " || k === "d") toggleMark(state.viewing);
      else return;
      ev.preventDefault();
      return;
    }

    switch (k) {
      case "ArrowRight": setFocus(state.focus + 1); break;
      case "ArrowLeft": setFocus(state.focus - 1); break;
      case "ArrowDown": setFocus(vertical(1)); break;
      case "ArrowUp": setFocus(vertical(-1)); break;
      case " ": case "d": toggleMark(state.focus); break;
      case "a": toggleMarkAll(); break;
      case "Enter": case "f": openViewer(state.focus); break;
      case "t": trashPage(); break;
      case "u": undoLast(); break;
      case "n": case "PageDown": goNext(); break;
      case "p": case "PageUp": goPrev(); break;
      case "+": case "=": changeCols(1); break;
      case "-": changeCols(-1); break;
      case "e": openReview(); break;
      case "?": help.showModal(); break;
      default: return;
    }
    ev.preventDefault();
  });

  // ---- Start ----

  const heartbeat = setInterval(() => {
    if (!state.stopped) api("/api/ping", {}).catch(() => {});
  }, HEARTBEAT_MS);

  // Tab closing (or reloading): say goodbye so the tool stops within seconds
  // instead of running on. A beacon is still delivered as the page goes away,
  // but can't carry headers, so the token goes in the body.
  window.addEventListener("pagehide", () => {
    if (state.stopped || state.screen === "ended") return;
    navigator.sendBeacon("/api/bye", new Blob([JSON.stringify({ token })], { type: "text/plain" }));
  });

  sizeSel.value = String(state.cols);
  // Marks and the undo list first (they may be left over from last time),
  // then the page where you stopped.
  (async () => {
    let sessionError = null;
    try { applySession(await api("/api/marks", {})); } catch (e) { sessionError = e; }
    await load(null);
    if (sessionError) {
      setStatus(`Could not load marks: ${sessionError.message}`, "error");
    } else if (state.marked.size || state.trashed.length) {
      const parts = [];
      if (state.marked.size) parts.push(`${plural(state.marked.size, "photo")} still marked`);
      if (state.trashed.length) parts.push(`${state.trashed.length} trashed (undo still possible)`);
      setStatus(`From last time: ${parts.join(", ")}.`);
    }
  })();
})();
