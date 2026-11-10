// Adds a "Download" button next to Like/Share on YouTube watch pages.
// YouTube is a single-page app and rebuilds this row often, so re-add the button whenever it is gone.
const LABELS = { OK: "Queued", OFF: "App closed", ERR: "Error", "YT?": "Not a video" };

function addButton() {
  if (location.pathname !== "/watch" || document.getElementById("ytgrab-btn")) return;
  const row = document.querySelector("ytd-watch-metadata #top-level-buttons-computed");
  if (!row) return;

  const btn = document.createElement("button");
  btn.id = "ytgrab-btn";
  btn.type = "button";
  btn.title = "Download with ytgrab";
  const icon = document.createElement("img");
  icon.src = chrome.runtime.getURL("icons/32.png");
  icon.alt = "";
  const label = document.createElement("span");
  label.textContent = "Download";
  label.setAttribute("aria-live", "polite"); // screen readers hear "Queued", "App closed", ...
  btn.append(icon, label);

  // aria-disabled, not disabled: a disabled button drops keyboard focus to the page.
  btn.addEventListener("click", async () => {
    if (btn.getAttribute("aria-disabled") === "true") return;
    btn.setAttribute("aria-disabled", "true");
    let code;
    try {
      code = await chrome.runtime.sendMessage({ type: "download" });
    } catch {
      code = "ERR"; // extension was reloaded; the page needs a refresh
    }
    label.textContent = LABELS[code] ?? "Error";
    setTimeout(() => {
      label.textContent = "Download";
      btn.removeAttribute("aria-disabled");
    }, 3000);
  });
  row.append(btn);
}

new MutationObserver(addButton).observe(document.documentElement, { childList: true, subtree: true });
addButton();
