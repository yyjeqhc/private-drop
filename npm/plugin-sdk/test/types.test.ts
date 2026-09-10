import {
  defineTool,
  schema,
  textResult,
  type InferSchema,
} from "../src/index.js";

const input = schema.object({
  text: schema.string(),
  count: schema.optional(schema.integer()),
});

defineTool({
  name: "inference",
  inputSchema: input,
  execute(args) {
    const text: string = args.text;
    const count: number | undefined = args.count;
    void text;
    void count;
    // @ts-expect-error unknown properties are rejected for closed object schemas.
    void args.missing;
    return textResult(args.text, { text: args.text });
  },
});

const modeSchema = schema.string({ enum: ["fast", "safe"] });
type Mode = InferSchema<typeof modeSchema>;
const mode: Mode = "fast";
void mode;
// @ts-expect-error enum literal inference excludes other strings.
const invalidMode: Mode = "other";
void invalidMode;

const exactSchema = schema.integer({ const: 2 });
type Exact = InferSchema<typeof exactSchema>;
const exact: Exact = 2;
void exact;
// @ts-expect-error const literal inference keeps the exact numeric value.
const invalidExact: Exact = 3;
void invalidExact;

const output = schema.object({ text: schema.string() });
defineTool({
  name: "typed-output",
  inputSchema: schema.object({}),
  outputSchema: output,
  execute() {
    return textResult("ok", { text: "ok" });
  },
});

defineTool({
  name: "bad-output",
  inputSchema: schema.object({}),
  outputSchema: output,
  // @ts-expect-error structuredContent must match the declared output schema at authoring time.
  execute() {
    return textResult("bad", { text: 42 });
  },
});

defineTool({
  name: "bad-input-root",
  // @ts-expect-error Plugin inputSchema roots must be object schemas.
  inputSchema: schema.string(),
  execute() {
    return textResult("bad");
  },
});

defineTool({
  name: "bad-output-root",
  inputSchema: schema.object({}),
  // @ts-expect-error Plugin outputSchema roots must be object schemas.
  outputSchema: schema.array(schema.string()),
  execute() {
    return textResult("bad", {});
  },
});

const openInput = schema.object(
  { known: schema.string() },
  { additionalProperties: true },
);
defineTool({
  name: "open-input",
  inputSchema: openInput,
  execute(args) {
    const known: string = args.known;
    const extra: unknown = args.other;
    void known;
    void extra;
    return textResult("ok");
  },
});
