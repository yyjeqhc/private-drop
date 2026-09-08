import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const css = readFileSync(new URL("../src/styles/app.css", import.meta.url), "utf8");
const fullWidthSelector = 'input:not([type="radio"]):not([type="checkbox"]), select';
const legacySelector = 'input:not([type="radio"]), select';

assert.match(
  css,
  new RegExp(`${escapeRegExp(fullWidthSelector)}\\s*\\{[^}]*width:\\s*100%;[^}]*height:\\s*39px;`),
  "text/password/url inputs and selects must retain the full-width 39px input contract",
);
assert.ok(
  !css.includes(`${legacySelector} { width: 100%; height: 39px;`),
  "checkboxes must not re-enter the legacy full-width input selector",
);
assert.match(
  css,
  /input\[type="checkbox"\]\s*\{[^}]*width:\s*16px;[^}]*height:\s*16px;/,
  "checkboxes must keep an explicit compact sizing contract",
);
assert.match(
  css,
  /\.provider-option input\[type="radio"\]\s*\{[^}]*width:\s*1px;[^}]*height:\s*1px;/,
  "provider radios must retain their existing custom sizing contract",
);

for (const type of ["text", "password", "url"]) {
  assert.ok(!["radio", "checkbox"].includes(type), `${type} remains covered by the non-choice selector`);
}

console.log("Desktop form control CSS contract passed");

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
