import {
  definePlugin,
  defineTool,
  runPlugin,
  schema,
  textResult,
} from "@yyjeqhc/webcodex-plugin-sdk";

const echo = defineTool({
  name: "echo",
  description: "Echo one string",
  inputSchema: schema.object({
    text: schema.string({ minLength: 1, maxLength: 4096 }),
  }),
  outputSchema: schema.object({
    text: schema.string({ maxLength: 4096 }),
  }),
  async execute({ text }) {
    await Promise.resolve();
    return textResult(text, { text });
  },
});

runPlugin(definePlugin({ tools: [echo] }));
