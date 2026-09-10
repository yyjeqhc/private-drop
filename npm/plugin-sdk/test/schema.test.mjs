import assert from "node:assert/strict";
import test from "node:test";

import { schema } from "../dist/index.js";

const PROFILE_KEYS = new Set([
  "type",
  "title",
  "description",
  "properties",
  "required",
  "additionalProperties",
  "enum",
  "const",
  "minLength",
  "maxLength",
  "minItems",
  "maxItems",
  "items",
]);

function assertProfileOnly(value) {
  assert.equal(typeof value, "object");
  assert.notEqual(value, null);
  assert.equal(Array.isArray(value), false);
  for (const key of Object.keys(value)) {
    assert.equal(PROFILE_KEYS.has(key), true, `unexpected schema keyword: ${key}`);
  }
  if (value.properties) {
    for (const child of Object.values(value.properties)) assertProfileOnly(child);
  }
  if (value.items) assertProfileOnly(value.items);
}

test("schema builders emit the Native Plugin Schema Profile v1 shapes", () => {
  const value = schema.object({
    text: schema.string({ minLength: 1, maxLength: 12, enum: ["a", "b"], const: "a" }),
    ratio: schema.number({ title: "Ratio" }),
    count: schema.integer({ description: "Count" }),
    enabled: schema.boolean(),
    empty: schema.null(),
    tags: schema.array(schema.string(), { minItems: 1, maxItems: 3 }),
  });

  assert.deepEqual(value, {
    type: "object",
    properties: {
      text: { type: "string", minLength: 1, maxLength: 12, enum: ["a", "b"], const: "a" },
      ratio: { type: "number", title: "Ratio" },
      count: { type: "integer", description: "Count" },
      enabled: { type: "boolean" },
      empty: { type: "null" },
      tags: { type: "array", items: { type: "string" }, minItems: 1, maxItems: 3 },
    },
    required: ["text", "ratio", "count", "enabled", "empty", "tags"],
    additionalProperties: false,
  });
  assertProfileOnly(value);
});

test("optional properties are omitted from required and object defaults closed", () => {
  const value = schema.object({
    requiredValue: schema.string(),
    optionalValue: schema.optional(schema.boolean()),
  });
  assert.deepEqual(value.required, ["requiredValue"]);
  assert.equal(value.additionalProperties, false);
  assert.deepEqual(value.properties.optionalValue, { type: "boolean" });
  assert.equal("optional" in value.properties.optionalValue, false);
});

test("additionalProperties true is explicit and enum/const literals stay on wire", () => {
  const value = schema.object(
    {
      mode: schema.string({ enum: ["fast", "safe"] }),
      exact: schema.integer({ const: 2 }),
    },
    { additionalProperties: true },
  );
  assert.equal(value.additionalProperties, true);
  assert.deepEqual(value.properties.mode.enum, ["fast", "safe"]);
  assert.equal(value.properties.exact.const, 2);
  assertProfileOnly(value);
});

test("schemas are immutable snapshots of authoring input", () => {
  const values = ["first", "second"];
  const value = schema.string({ enum: values });
  values.push("later");
  assert.deepEqual(value.enum, ["first", "second"]);
  assert.equal(Object.isFrozen(value), true);
  assert.equal(Object.isFrozen(value.enum), true);
});
