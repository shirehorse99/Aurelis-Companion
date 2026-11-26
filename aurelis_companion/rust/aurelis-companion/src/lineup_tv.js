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

  function drawCountryOrder(countries) {
    const box = $("country-order");
    if (!box) return;
    if (countries.length < 2) { box.replaceChildren(el("p", { class: "job-meta", text: "Only one country so far." })); return; }
    const move = (i, by) => {
      const order = countries.slice();
      const [c] = order.splice(i, 1);
      order.splice(i + by, 0, c);
      call("ui/country_order", Object.assign(tabRef(), { order }));
    };
    box.replaceChildren(
      el("p", { class: "job-meta", text: "Each television puts its own home country first, then this order." }),
      el("ol", { class: "country-list" }, ...countries.map((c, i) => el("li", {},
        el("span", { class: "cc-name", text: c }),
        editor ? el("button", { type: "button", class: "mv small", "aria-label": "Move " + c + " up", disabled: i === 0, onclick: () => move(i, -1) }, "▲") : null,
        editor ? el("button", { type: "button", class: "mv small", "aria-label": "Move " + c + " down", disabled: i === countries.length - 1, onclick: () => move(i, 1) }, "▼") : null,
      ))),
    );
  }

  const household = () => data.household || { categories: [], folders: [] };
  const isHouseholdKey = (k) => /^HC_\d+$/.test(k || "");
  const householdFolders = (key) => household().folders.filter((f) => f.category === key).map((f) => f.path);
  const folderLabel = (path) => path.replace(/::/g, " / ");
  const leaf = (path) => path.split("::").pop();
  function categoryLabel(key) {
    const c = household().categories.find((x) => x.key === key);
    if (c) return c.name;
    const g = BOOT.groups.find((x) => x.key === key);
    if (g) return g.label;
    return key === "SPORT_EXTRA" ? "Sports Extra" : key;
  }
  function groupsCall(action, fields) {
    return call("ui/groups", Object.assign(tabRef(), { action }, fields));
  }
  function askName(question, current) {
    const v = prompt(question, current || "");
    return v === null ? null : v.trim();
  }

  // The side list: every category as the televisions list it, its folders under it. Built-in
  // categories carry a padlock; the household's own can be renamed and deleted, and any
  // category can hold folders.
  function drawPickers() {
    const countries = [...new Set(data.tabs.map((t) => t.country))];
    drawCountryOrder(countries.filter((c) => c !== "UNKNOWN"));
    const sel = $("country");
    sel.replaceChildren(...countries.map((c) => el("option", { value: c, text: c === "UNKNOWN" ? "No country" : c, selected: data.tab && c === data.tab.country })));
    sel.onchange = () => {
      const first = data.tabs.find((t) => t.country === sel.value && t.group === (data.tab && data.tab.group)) || data.tabs.find((t) => t.country === sel.value);
      if (first) reload(first);
    };
    const country = (data.tab && data.tab.country) || countries[0] || "GB";
    const here = data.tabs.filter((t) => t.country === country);
    const isActive = (key, folder) => data.tab && data.tab.group === key && (data.tab.folder || "") === (folder || "");
    const open = (key, folder) => reload(folder ? { country, group: key, folder } : { country, group: key });
    const keys = [...new Set(here.map((t) => t.group))];
    for (const c of household().categories) if (!keys.includes(c.key)) keys.push(c.key);

    const items = [];
    const options = [];
    const tool = (label, title, fn) => el("button", { type: "button", class: "mv small tool", title, "aria-label": title, onclick: (e) => { e.stopPropagation(); fn(); } }, label);
    for (const key of keys) {
      const tabs = here.filter((t) => t.group === key);
      const root = tabs.find((t) => !t.folder);
      const label = categoryLabel(key);
      const mine = isHouseholdKey(key);
      const listed = householdFolders(key);
      const folders = [...new Set(tabs.filter((t) => t.folder).map((t) => t.folder).concat(listed))]
        .sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
      const count = (t) => (t ? t.count - t.hidden : 0);
      const total = tabs.reduce((n, t) => n + count(t), 0);
      const tools = [];
      if (editor) {
        if (mine) {
          tools.push(tool("✎", "Rename " + label, () => {
            const name = askName("New name for " + label + ":", label);
            if (name) groupsCall("renameCategory", { category: key, name });
          }));
          tools.push(tool("✕", "Delete " + label, () => {
            const ask = total
              ? "Delete " + label + "? Its " + total + " channel(s) go back to the groups Aurelis puts them in, without dial numbers."
              : "Delete " + label + "?";
            if (confirm(ask)) groupsCall("deleteCategory", { category: key });
          }));
        } else {
          tools.push(el("span", { class: "lock", title: label + " is built in: it cannot be renamed or deleted. Its folders can be.", "aria-label": "Built in" }, "🔒"));
        }
        if (key !== "HIDDEN") {
          tools.push(tool("+", "Add a folder to " + label, () => {
            const name = askName("Name of the new folder in " + label + ":");
            if (name) groupsCall("addFolder", { category: key, name });
          }));
        }
      }
      // With folders, the category's own row opens the channels in no folder, so it counts those.
      const own = folders.length ? count(root) : total;
      const head = el("div", { class: "group-row" + (isActive(key, null) ? " active" : "") },
        el("button", { type: "button", class: "group-item" + (isActive(key, null) ? " active" : "") + (folders.length ? " parent" : ""), "aria-current": isActive(key, null) ? "true" : false, onclick: () => open(key, null),
          title: folders.length ? total + " in all; " + own + " in no folder" : null },
          el("span", { text: label }), el("span", { class: "count", text: folders.length ? own + " of " + total : String(total) })),
        ...tools);
      items.push(head);
      options.push(el("option", { value: JSON.stringify([key, ""]), selected: isActive(key, null), text: label + " (" + total + ")" }));
      for (const folder of folders) {
        const t = tabs.find((x) => x.folder === folder);
        const depth = folder.split("::").length;
        const editable = editor && (key === "SPORT_EXTRA" || listed.includes(folder)) && key !== "OTHERS";
        const ftools = [];
        if (editable) {
          ftools.push(tool("✎", "Rename the folder " + leaf(folder), () => {
            const name = askName("New name for the folder " + leaf(folder) + ":", leaf(folder));
            if (name) groupsCall("renameFolder", { category: key, path: folder, name });
          }));
          ftools.push(tool("✕", "Delete the folder " + leaf(folder), () => {
            const n = folders.filter((f) => f === folder || f.startsWith(folder + "::"))
              .reduce((sum, f) => sum + count(tabs.find((x) => x.folder === f)), 0);
            if (confirm("Delete the folder " + folderLabel(folder) + " and any folders inside it? " + n + " channel(s) move up a level.")) {
              groupsCall("deleteFolder", { category: key, path: folder });
            }
          }));
          if (depth < 3) ftools.push(tool("+", "Add a folder inside " + leaf(folder), () => {
            const name = askName("Name of the new folder inside " + leaf(folder) + ":");
            if (name) groupsCall("addFolder", { category: key, path: folder, name });
          }));
        }
        items.push(el("div", { class: "group-row sub depth-" + depth + (isActive(key, folder) ? " active" : "") },
          el("button", { type: "button", class: "group-item sub" + (isActive(key, folder) ? " active" : ""), "aria-current": isActive(key, folder) ? "true" : false, onclick: () => open(key, folder) },
            el("span", { text: leaf(folder) }), el("span", { class: "count", text: String(count(t)) })),
          ...ftools));
        options.push(el("option", { value: JSON.stringify([key, folder]), selected: isActive(key, folder), text: label + " / " + folderLabel(folder) + " (" + count(t) + ")" }));
      }
    }
    if (editor) {
      items.push(el("button", { type: "button", class: "group-add", onclick: () => {
        const name = askName("Name of the new category (a guide tab on every television):");
        if (name) groupsCall("addCategory", { name });
      } }, "+ New category"));
    }
    $("groups").replaceChildren(...items);
    const gsel = $("group-select");
    gsel.replaceChildren(...options);
    gsel.onchange = () => { const [key, folder] = JSON.parse(gsel.value); open(key, folder); };
  }

  // Every category and folder a channel can be moved to, from the live list.
  function moveOptions() {
    const out = [];
    const keys = BOOT.groups.map((g) => g.key).filter((k) => k !== "HIDDEN");
    keys.splice(keys.indexOf("SPORT") + 1, 0, "SPORT_EXTRA");
    for (const c of household().categories) keys.push(c.key);
    keys.push("HIDDEN");
    for (const key of keys) {
      const label = categoryLabel(key);
      if (key !== "SPORT_EXTRA") out.push(el("option", { value: key, text: label }));
      let folders = householdFolders(key);
      if (key === "SPORT_EXTRA") folders = [...new Set(BOOT.folders.concat(folders))];
      for (const f of folders.sort()) {
        out.push(el("option", { value: key + "::" + f, text: label + " / " + folderLabel(f) }));
      }
    }
    return out;
  }

  function drawStatus() {
    const s = $("tab-status");
    if (!data.tab) { s.textContent = "No channels have been reported by a television yet."; return; }
    const shown = data.tab.count - data.tab.hidden;
    const hasFolders = data.tabs.some((t) => t.country === data.tab.country && t.group === data.tab.group && t.folder)
      || householdFolders(data.tab.group).length > 0;
    const title = categoryLabel(data.tab.group) + (data.tab.folder ? " / " + folderLabel(data.tab.folder) : (hasFolders ? " (in no folder)" : ""))
      + " · " + (data.tab.country === "UNKNOWN" ? "No country" : data.tab.country);
    s.replaceChildren(
      el("h2", { text: title }),
      el("div", { class: "tab-facts" },
        el("span", { text: shown + " on the televisions" + (data.tab.hidden ? ", " + data.tab.hidden + " hidden" : "") }),
        el("span", {
          class: "badge " + (data.tab.managed ? "done" : "pending"),
          title: data.tab.managed
            ? "Every television shows this group exactly in this order."
            : "Each television sorts this group itself; this is as close as the Companion can show it. Move any channel to fix the order for every television.",
          text: data.tab.managed ? "Your order" : "Automatic order",
        }),
      ),
    );
    const release = document.querySelector('#group-action option[value="release"]');
    if (release) release.hidden = !data.tab.managed;
  }

  let dragId = null;
  function row(c, i, n) {
    const name = el("button", { type: "button", class: "ch-name", title: editor ? "Edit " + c.name : c.name, onclick: editor ? () => toggleEdit(c.id) : null, disabled: !editor },
      c.name);
    const meta = [];
    if (c.kind) meta.push(el("span", { class: "badge", title: "Shown here because the televisions put " + c.kind + " channels in Entertainment; they also appear in the " + c.kind + " tab, in this order.", text: c.kind }));
    if (c.hidden) meta.push(el("span", { class: "badge pending", text: "Hidden" }));
    const plain = (x) => (x || "").toLowerCase().replace(/[^a-z0-9+]/g, "");
    if (c.originalName && plain(c.originalName) !== plain(c.name)) meta.push(el("span", { class: "job-meta", text: "was " + c.originalName }));
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
      // Dial numbers are per country: Britain's 101 is not Australia's 101.
      el("span", { class: "dial", text: c.number != null ? ((data.tab && data.tab.country !== "UNKNOWN" ? data.tab.country + " " : "") + c.number) : "–" }),
      c.logo ? el("img", { class: "ch-logo", src: c.logo, alt: "", loading: "lazy", referrerpolicy: "no-referrer" }) : el("span", { class: "ch-logo" }),
      el("span", { class: "ch-main" }, name, el("span", { class: "ch-meta" }, ...meta)),
    );
    if (editor) {
      const b = (label, title, fn, disabled) => el("button", { type: "button", class: "mv", title, "aria-label": title + ": " + c.name, onclick: fn, disabled }, label);
      li.append(el("span", { class: "moves" },
        b("▲", "Up one", () => moveTo(c.id, i - 1), i === 0),
        b("▼", "Down one", () => moveTo(c.id, i + 1), i === n - 1),
        b("⋯", "More", () => toggleEdit(c.id), false),
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

  function editPanel(c, i, n) {
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
      ...moveOptions(),
    );
    groupSel.onchange = () => { if (groupSel.value !== "__") send({ moveToGroup: groupSel.value }); };
    const placeIn = el("input", { type: "number", min: "1", max: String(n), value: String(i + 1), "aria-label": "Place in this group", class: "num" });
    onEnterOrBlur(placeIn, (v) => {
      const p = parseInt(v, 10);
      if (!(p >= 1 && p <= n)) { toast("Enter a place from 1 to " + n + ".", true); return; }
      moveTo(c.id, p - 1);
    });
    return el("div", { class: "edit-panel" },
      el("div", { class: "edit-place" },
        el("span", { class: "job-meta", text: "Place" }),
        el("button", { type: "button", disabled: i === 0, onclick: () => moveTo(c.id, 0) }, "⤒ To top"),
        el("button", { type: "button", disabled: i === n - 1, onclick: () => moveTo(c.id, n) }, "⤓ To bottom"),
        el("label", {}, "Move to place ", placeIn),
      ),
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
      if (openId === c.id && editor) li.append(editPanel(c, i, n));
      rows.push(li);
    });
    if (!rows.length) rows.push(el("li", { class: "job-meta empty-note", text: n ? "No channel in this group matches."
      : (data.tab && (isHouseholdKey(data.tab.group) || data.tab.folder)
        ? "Nothing here yet, so the televisions do not show it. Open a channel's ⋯ in another group and choose this one under Group."
        : "This group is empty.") }));
    list.replaceChildren(...rows);
    list.classList.toggle("filtered", !!find);
  }

  function draw() {
    drawPickers();
    drawStatus();
    drawList();
  }

  if (editor) {
    const action = $("group-action");
    action.addEventListener("change", () => {
      const sort = action.value;
      const label = action.options[action.selectedIndex].text;
      action.value = "";
      if (!sort) return;
      const ask = sort === "release"
        ? "Hand this group back to each television's automatic order? Its dial numbers are cleared."
        : "Re-order the whole group (" + label + ")? Dial numbers follow the new order.";
      if (confirm(ask)) call("ui/tab_sort", Object.assign(tabRef(), { sort }));
    });
    $("find").addEventListener("input", drawList);
  }
  draw();
})();
