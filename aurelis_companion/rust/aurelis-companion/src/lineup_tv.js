"use strict";
// The channel editor, drawn as the televisions draw a tab. Every action posts at once and the
// page redraws from the Companion's answer, so the screen is always what the televisions get.
(function () {
  const editor = BOOT.editor;
  let data = BOOT.data;
  let busy = false;
  const $ = (id) => document.getElementById(id);
  const el = (tag, attrs, ...kids) => {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (k === "class") n.className = v;
      else if (k === "text") n.textContent = v;
      else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
      else if (v !== false && v !== null && v !== undefined) n.setAttribute(k, v === true ? "" : v);
    }
    for (const kid of kids) if (kid !== null && kid !== undefined) n.append(kid);
    return n;
  };

  function tabRef(t) {
    t = t || data.tab;
    const ref = { country: t.country, group: t.group };
    if (t.folder) ref.folder = t.folder;
    return ref;
  }

  function toast(text, bad) {
    const t = $("toast");
    t.textContent = text;
    t.className = "toast show" + (bad ? " bad" : "");
    clearTimeout(toast.timer);
    toast.timer = setTimeout(() => { t.className = "toast"; }, bad ? 8000 : 3500);
  }

  async function call(path, body) {
    if (busy) return;
    busy = true;
    document.body.classList.add("saving");
    try {
      const resp = await fetch(path, {
        method: body ? "POST" : "GET",
        headers: body ? { "Content-Type": "application/json" } : {},
        body: body ? JSON.stringify(body) : undefined,
        credentials: "same-origin",
      });
      let answer = null;
      try { answer = await resp.json(); } catch (e) { /* not JSON */ }
      if (!resp.ok || !answer || answer.ok === false) {
        const why = (answer && answer.reason) || ("The Companion answered HTTP " + resp.status + ".");
        toast("Not saved: " + why, true);
        await reload();
        return;
      }
      data = answer;
      draw();
      if (answer.message) toast(answer.message);
    } catch (e) {
      toast("Could not reach the Companion: " + e.message, true);
    } finally {
      busy = false;
      document.body.classList.remove("saving");
    }
  }

  async function reload(t) {
    const ref = tabRef(t);
    const q = new URLSearchParams(ref).toString();
    const resp = await fetch("ui/tab?" + q, { credentials: "same-origin" });
    data = await resp.json();
    draw();
    history.replaceState(null, "", "lineup?" + q);
  }

  const ids = () => data.channels.map((c) => c.id);
  function moveTo(id, index) {
    const order = ids().filter((x) => x !== id);
    index = Math.max(0, Math.min(order.length, index));
    order.splice(index, 0, id);
    return call("ui/tab_order", Object.assign(tabRef(), { ids: order }));
  }

  function drawPickers() {
    const countries = [...new Set(data.tabs.map((t) => t.country))];
    const sel = $("country");
    sel.replaceChildren(...countries.map((c) => el("option", { value: c, text: c === "UNKNOWN" ? "No country" : c, selected: data.tab && c === data.tab.country })));
    sel.onchange = () => {
      const first = data.tabs.find((t) => t.country === sel.value && t.group === (data.tab && data.tab.group)) || data.tabs.find((t) => t.country === sel.value);
      if (first) reload(first);
    };
    const here = data.tabs.filter((t) => data.tab && t.country === data.tab.country);
    const isActive = (t) => data.tab && t.group === data.tab.group && (t.folder || "") === (data.tab.folder || "");
    const chip = (t, label, count, active) => el("button", { type: "button", role: "tab", "aria-selected": active ? "true" : "false", class: "chip" + (active ? " active" : ""), onclick: () => reload(t) },
      label, el("span", { class: "count", text: String(count) }));
    const extras = here.filter((t) => t.group === "SPORT_EXTRA");
    const chips = [];
    let extraShown = false;
    for (const t of here) {
      if (t.group !== "SPORT_EXTRA") { chips.push(chip(t, t.label, t.count - t.hidden, isActive(t))); continue; }
      if (extraShown) continue;
      extraShown = true;
      const total = extras.reduce((n, x) => n + x.count - x.hidden, 0);
      chips.push(chip(extras[0], "Sports Extra", total, data.tab.group === "SPORT_EXTRA"));
    }
    const folderRow = $("folders");
    if (data.tab && data.tab.group === "SPORT_EXTRA" && extras.length > 1) {
      folderRow.hidden = false;
      folderRow.replaceChildren(el("span", { class: "job-meta", text: "Folder:" }),
        ...extras.map((t) => chip(t, t.folder ? t.folder.replace(/::/g, " / ") : "No folder", t.count - t.hidden, isActive(t))));
    } else {
      folderRow.hidden = true;
      folderRow.replaceChildren();
    }
    $("groups").replaceChildren(...chips);
  }

  function drawStatus() {
    const s = $("tab-status");
    if (!data.tab) { s.textContent = "No channels have been reported by a television yet."; return; }
    const shown = data.tab.count - data.tab.hidden;
    const hiddenNote = data.tab.hidden ? " · " + data.tab.hidden + " hidden (greyed out below)" : "";
    s.replaceChildren(
      el("strong", { text: shown + " channels on the televisions" + hiddenNote }),
      el("span", { class: "badge " + (data.tab.managed ? "done" : "pending"), text: data.tab.managed ? "Your order" : "Automatic order" }),
      el("span", { class: "job-meta", text: data.tab.managed
        ? "Every television shows this group exactly in this order."
        : "Each television sorts this group itself; shown here as closely as the Companion can (a box may also lift its most-watched channels). Move any channel to fix the order for every television." }),
    );
    const release = document.querySelector('[data-sort="release"]');
    if (release) release.hidden = !data.tab.managed;
  }

  let dragId = null;
  function row(c, i, n) {
    const name = el("button", { type: "button", class: "ch-name", title: editor ? "Edit " + c.name : c.name, onclick: editor ? () => toggleEdit(c.id) : null, disabled: !editor },
      c.name);
    const meta = [];
    if (c.hidden) meta.push(el("span", { class: "badge pending", text: "Hidden" }));
    if (c.originalName) meta.push(el("span", { class: "job-meta", text: "was " + c.originalName }));
    if (c.edited) meta.push(el("span", { class: "badge edited", text: "Edited" }));
    if (c.twins && c.twins.length) {
      const boxes = [c.by].concat(c.twins.map((t) => t.by)).map((b) => b || "an earlier report");
      meta.push(el("span", {
        class: "badge twin",
        title: "The same channel under " + (c.twins.length + 1) + " ids, reported by " + [...new Set(boxes)].join(", ") + ". Every change here is made to all of them.",
        text: (c.twins.length + 1) + " ids",
      }));
      if (c.sameBox) meta.push(el("span", {
        class: "badge pending",
        title: "This television's own channel merge keeps these apart, so it shows this channel " + (c.twins.length + 1) + " times. That is a merge rule to fix on the television, not here.",
        text: "Shown twice on " + c.by,
      }));
    }
    const li = el("li", { class: "tv-row" + (c.hidden ? " is-hidden" : ""), "data-id": c.id, draggable: editor ? "true" : false },
      editor ? el("span", { class: "grip", "aria-hidden": "true", text: "⠿" }) : null,
      el("span", { class: "dial", text: c.number != null ? String(c.number) : "–" }),
      c.logo ? el("img", { class: "ch-logo", src: c.logo, alt: "", loading: "lazy", referrerpolicy: "no-referrer" }) : el("span", { class: "ch-logo" }),
      el("span", { class: "ch-main" }, name, el("span", { class: "ch-meta" }, ...meta)),
    );
    if (editor) {
      const b = (label, title, fn, disabled) => el("button", { type: "button", class: "mv", title, "aria-label": title + ": " + c.name, onclick: fn, disabled }, label);
      li.append(el("span", { class: "moves" },
        b("⤒", "To top", () => moveTo(c.id, 0), i === 0),
        b("▲", "Up one", () => moveTo(c.id, i - 1), i === 0),
        b("▼", "Down one", () => moveTo(c.id, i + 1), i === n - 1),
        b("⤓", "To bottom", () => moveTo(c.id, n), i === n - 1),
        b("#", "Move to place", () => {
          const v = prompt("Move " + c.name + " to place (1 to " + n + "):", String(i + 1));
          if (v === null) return;
          const p = parseInt(v, 10);
          if (!(p >= 1 && p <= n)) { toast("Enter a place from 1 to " + n + ".", true); return; }
          moveTo(c.id, p - 1);
        }),
      ));
      li.addEventListener("dragstart", (e) => { dragId = c.id; li.classList.add("dragging"); e.dataTransfer.effectAllowed = "move"; e.dataTransfer.setData("text/plain", c.id); });
      li.addEventListener("dragend", () => { li.classList.remove("dragging"); document.querySelectorAll(".drop-before,.drop-after").forEach((x) => x.classList.remove("drop-before", "drop-after")); });
      li.addEventListener("dragover", (e) => {
        if (!dragId || dragId === c.id) return;
        e.preventDefault();
        const after = e.offsetY > li.offsetHeight / 2;
        li.classList.toggle("drop-after", after);
        li.classList.toggle("drop-before", !after);
      });
      li.addEventListener("dragleave", () => li.classList.remove("drop-before", "drop-after"));
      li.addEventListener("drop", (e) => {
        e.preventDefault();
        if (!dragId || dragId === c.id) return;
        const after = li.classList.contains("drop-after");
        const order = ids().filter((x) => x !== dragId);
        const at = order.indexOf(c.id) + (after ? 1 : 0);
        const moving = dragId;
        dragId = null;
        moveTo(moving, at);
      });
    }
    return li;
  }

  let openId = null;
  function toggleEdit(id) {
    openId = openId === id ? null : id;
    drawList();
  }

  function editPanel(c) {
    const send = (fields) => call("ui/channel_edit", Object.assign(tabRef(), { channelId: c.id }, fields));
    const onEnterOrBlur = (input, fn) => {
      let last = input.value;
      const go = () => { if (input.value !== last) { last = input.value; fn(input.value); } };
      input.addEventListener("change", go);
      input.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); go(); } });
    };
    const nameIn = el("input", { type: "text", value: c.name, "aria-label": "Name" });
    onEnterOrBlur(nameIn, (v) => send({ displayName: v }));
    const numIn = el("input", { type: "number", min: "1", value: c.number != null ? c.number : "", "aria-label": "Dial number", class: "num" });
    onEnterOrBlur(numIn, (v) => send({ number: v }));
    const countryIn = el("input", { type: "text", value: c.country || "", maxlength: "2", "aria-label": "Country", class: "cc" });
    onEnterOrBlur(countryIn, (v) => send({ countryCode: v.toUpperCase() }));
    const logoIn = el("input", { type: "url", value: "", placeholder: "https://… (blank: provider logo)", "aria-label": "Logo URL" });
    onEnterOrBlur(logoIn, (v) => send({ logoUrl: v }));
    const groupSel = el("select", { "aria-label": "Move to group" },
      el("option", { value: "__", text: "Move to another group…" }),
      el("option", { value: "", text: "Automatic (as the provider files it)" }),
      ...BOOT.groups.map((g) => el("option", { value: g.key, text: g.label })),
      ...BOOT.folders.map((f) => el("option", { value: "SPORT_EXTRA::" + f, text: "Sports Extra / " + f.replace(/::/g, " / ") })),
    );
    groupSel.onchange = () => { if (groupSel.value !== "__") send({ moveToGroup: groupSel.value }); };
    return el("div", { class: "edit-panel" },
      el("label", {}, "Name ", nameIn),
      el("label", {}, "Dial # ", numIn),
      el("label", {}, "Country ", countryIn),
      el("label", {}, "Group ", groupSel),
      el("label", { class: "wide" }, "Logo ", logoIn),
      el("div", { class: "edit-actions" },
        el("button", { type: "button", onclick: () => send({ hidden: !c.hidden }) }, c.hidden ? "Show on televisions" : "Hide from televisions"),
        c.edited ? el("button", { type: "button", class: "quiet", onclick: () => { if (confirm("Undo the name, group, country, logo and hiding changes to " + c.name + "? Its place in the order stays.")) send({ reset: true }); } }, "Undo changes") : null,
        el("button", { type: "button", class: "quiet", onclick: () => toggleEdit(c.id) }, "Close"),
      ),
      el("p", { class: "job-meta", text: "Each box saves when you press Enter or leave it. Provider category: " + (c.provider || "none")
        + (c.twins && c.twins.length ? ". Applies to all " + (c.twins.length + 1) + " ids of this channel." : "") }),
    );
  }

  function drawList() {
    const list = $("list");
    const find = ($("find") && $("find").value.trim().toLowerCase()) || "";
    const n = data.channels.length;
    const rows = [];
    data.channels.forEach((c, i) => {
      if (find && !c.name.toLowerCase().includes(find) && String(c.number || "") !== find) return;
      const li = row(c, i, n);
      if (openId === c.id && editor) li.append(editPanel(c));
      rows.push(li);
    });
    if (!rows.length) rows.push(el("li", { class: "job-meta", text: n ? "No channel in this group matches." : "This group is empty." }));
    list.replaceChildren(...rows);
    list.classList.toggle("filtered", !!find);
  }

  function draw() {
    drawPickers();
    drawStatus();
    drawList();
  }

  if (editor) {
    document.querySelectorAll("[data-sort]").forEach((b) => b.addEventListener("click", () => {
      const sort = b.getAttribute("data-sort");
      const ask = sort === "release"
        ? "Hand this group back to each television's automatic order? Its dial numbers are cleared."
        : "Re-order the whole group (" + b.textContent + ")? Dial numbers follow the new order.";
      if (confirm(ask)) call("ui/tab_sort", Object.assign(tabRef(), { sort }));
    }));
    $("find").addEventListener("input", drawList);
  }
  draw();
})();
