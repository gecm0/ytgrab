import { toNetscape } from "./cookies.js";

const APP = "http://127.0.0.1:47321/job";
const YOUTUBE = /^https:\/\/((www|m|music)\.)?youtube\.com\/|^https:\/\/youtu\.be\//;
const COLORS = { OK: "#2a2", OFF: "#c22", ERR: "#c22", "YT?": "#888" };

// Queues a URL in the app. Returns OK, OFF (app not running), ERR, or YT? (not YouTube).
async function send(url) {
  if (!YOUTUBE.test(url)) return "YT?";
  const cookies = toNetscape(await chrome.cookies.getAll({ domain: "youtube.com" }));
  try {
    // Fetch from the service worker: its Origin is our pinned extension ID, which the app checks,
    // and host_permissions let it skip CORS.
    const res = await fetch(APP, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url, cookies }),
    });
    return res.ok ? "OK" : "ERR";
  } catch {
    return "OFF";
  }
}

chrome.action.onClicked.addListener(async (tab) => {
  const code = await send(tab.url ?? "");
  chrome.action.setBadgeText({ text: code, tabId: tab.id });
  chrome.action.setBadgeBackgroundColor({ color: COLORS[code], tabId: tab.id });
  setTimeout(() => chrome.action.setBadgeText({ text: "", tabId: tab.id }), 4000);
});

// From the in-page button. Use the tab URL Chrome reports, not anything the page sends.
chrome.runtime.onMessage.addListener((msg, sender, reply) => {
  if (msg?.type !== "download" || !sender.tab) return;
  send(sender.tab.url ?? "").then(reply);
  return true; // keep the channel open for the async reply
});
