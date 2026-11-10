// Run: bun extension/cookies.test.js
import { toNetscape } from "./cookies.js";

const out = toNetscape([
  { domain: ".youtube.com", path: "/", secure: true, httpOnly: true, session: false, expirationDate: 1893456000.7, name: "SID", value: "a" },
  { domain: "www.youtube.com", path: "/", secure: false, httpOnly: false, session: true, name: "PREF", value: "b" },
]);
const lines = out.trimEnd().split("\n");
const expect = [
  "# Netscape HTTP Cookie File",
  "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1893456000\tSID\ta",
  "www.youtube.com\tFALSE\t/\tFALSE\t0\tPREF\tb",
];
for (const [i, e] of expect.entries()) {
  if (lines[i] !== e) throw new Error(`line ${i}: ${JSON.stringify(lines[i])} != ${JSON.stringify(e)}`);
}
console.log("cookies ok");
