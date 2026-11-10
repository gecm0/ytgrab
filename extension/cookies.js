// Serializes chrome.cookies.Cookie objects to the Netscape cookies.txt format yt-dlp reads.
export function toNetscape(cookies) {
  const lines = ["# Netscape HTTP Cookie File"];
  for (const c of cookies) {
    // yt-dlp marks httpOnly cookies with this prefix on the domain field.
    const domain = (c.httpOnly ? "#HttpOnly_" : "") + c.domain;
    const sub = c.domain.startsWith(".") ? "TRUE" : "FALSE";
    const secure = c.secure ? "TRUE" : "FALSE";
    const expiry = c.session ? 0 : Math.floor(c.expirationDate ?? 0);
    lines.push([domain, sub, c.path, secure, expiry, c.name, c.value].join("\t"));
  }
  return lines.join("\n") + "\n";
}
