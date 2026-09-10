export type JsonPrimitive = null | boolean | number | string;
export type JsonValue =
  | JsonPrimitive
  | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };
export type JsonObject = { readonly [key: string]: JsonValue };

export type SchemaTypeName =
  | "object"
  | "array"
  | "string"
  | "number"
  | "integer"
  | "boolean"
  | "null";

export interface SchemaAnnotations {
  readonly title?: string;
  readonly description?: string;
  readonly enum?: readonly JsonValue[];
  readonly const?: JsonValue;
}

export interface ObjectSchemaShape extends SchemaAnnotations {
  readonly type: "object";
  readonly properties: Readonly<Record<string, SchemaNode>>;
  readonly required: readonly string[];
  readonly additionalProperties: boolean;
}

export interface ArraySchemaShape extends SchemaAnnotations {
  readonly type: "array";
  readonly items: SchemaNode;
  readonly minItems?: number;
  readonly maxItems?: number;
}

export interface StringSchemaShape extends SchemaAnnotations {
  readonly type: "string";
  readonly minLength?: number;
  readonly maxLength?: number;
}

export interface NumberSchemaShape extends SchemaAnnotations {
  readonly type: "number" | "integer";
}

export interface BooleanSchemaShape extends SchemaAnnotations {
  readonly type: "boolean";
}

export interface NullSchemaShape extends SchemaAnnotations {
  readonly type: "null";
}

export type SchemaNode =
  | ObjectSchemaShape
  | ArraySchemaShape
  | StringSchemaShape
  | NumberSchemaShape
  | BooleanSchemaShape
  | NullSchemaShape;

declare const schemaValue: unique symbol;

export type Schema<T, Shape extends SchemaNode = SchemaNode> = Readonly<Shape> & {
  readonly [schemaValue]?: T;
};

export type AnySchema = Schema<any, SchemaNode>;
export type ObjectSchema<T extends object = Record<string, unknown>> = Schema<T, ObjectSchemaShape>;

export type InferSchema<S> = S extends { readonly [schemaValue]?: infer T } ? T : never;

export interface OptionalSchema<S extends AnySchema> {
  readonly optional: true;
  readonly schema: S;
}

export type PropertySchema = AnySchema | OptionalSchema<AnySchema>;
export type PropertyMap = Readonly<Record<string, PropertySchema>>;

export interface PluginToolDefinition {
  readonly name: string;
  readonly title?: string;
  readonly description?: string;
  readonly inputSchema: ObjectSchemaShape;
  readonly outputSchema?: ObjectSchemaShape;
  readonly annotations?: JsonObject;
}

export interface TextContent {
  readonly type: "text";
  readonly text: string;
}

export interface ToolResult<TStructured extends object = object, TError extends boolean = boolean> {
  readonly content: readonly TextContent[];
  readonly structuredContent: TStructured;
  readonly isError: TError;
}

export type MaybePromise<T> = T | Promise<T>;

declare const definedToolBrand: unique symbol;
declare const pluginBrand: unique symbol;

export interface DefinedTool<
  TInput extends ObjectSchema<any> = ObjectSchema<any>,
  TOutput extends ObjectSchema<any> | undefined = ObjectSchema<any> | undefined,
> {
  readonly [definedToolBrand]: {
    readonly input: TInput;
    readonly output: TOutput;
  };
}

export interface Plugin {
  readonly [pluginBrand]: true;
}

export interface DefinePluginOptions<TTools extends readonly DefinedTool[]> {
  readonly tools: TTools;
}

type OutputValue<TOutput extends ObjectSchema<any> | undefined> =
  TOutput extends ObjectSchema<infer T> ? T : object;

export interface DefineToolOptions<
  TInput extends ObjectSchema<any>,
  TOutput extends ObjectSchema<any> | undefined,
> {
  readonly name: string;
  readonly title?: string;
  readonly description?: string;
  readonly inputSchema: TInput;
  readonly outputSchema?: TOutput;
  readonly annotations?: JsonObject;
  readonly execute: (
    args: InferSchema<TInput>,
  ) => MaybePromise<ToolResult<NoInfer<OutputValue<TOutput>>>>;
}

const TOOL_DEFINITION = Symbol("webcodex.plugin.tool-definition");
const TOOL_EXECUTE = Symbol("webcodex.plugin.tool-execute");
const PLUGIN_DEFINITIONS = Symbol("webcodex.plugin.definitions");
const PLUGIN_DISPATCH = Symbol("webcodex.plugin.dispatch");

interface InternalDefinedTool {
  readonly [TOOL_DEFINITION]: PluginToolDefinition;
  readonly [TOOL_EXECUTE]: (args: object) => MaybePromise<ToolResult<object>>;
}

interface InternalPlugin {
  readonly [PLUGIN_DEFINITIONS]: readonly PluginToolDefinition[];
  readonly [PLUGIN_DISPATCH]: Readonly<Record<string, InternalDefinedTool>>;
}

function cloneAndFreezeJson(value: JsonValue): JsonValue {
  if (Array.isArray(value)) {
    return Object.freeze(value.map((item) => cloneAndFreezeJson(item)));
  }
  if (value !== null && typeof value === "object") {
    const clone = Object.fromEntries(
      Object.entries(value).map(([key, child]) => [key, cloneAndFreezeJson(child)]),
    ) as Record<string, JsonValue>;
    return Object.freeze(clone);
  }
  return value;
}

function cloneDefinition(
  options: DefineToolOptions<ObjectSchema<any>, ObjectSchema<any> | undefined>,
): PluginToolDefinition {
  const definition: {
    name: string;
    title?: string;
    description?: string;
    inputSchema: ObjectSchemaShape;
    outputSchema?: ObjectSchemaShape;
    annotations?: JsonObject;
  } = {
    name: options.name,
    inputSchema: cloneAndFreezeJson(options.inputSchema as unknown as JsonValue) as unknown as ObjectSchemaShape,
  };
  if (options.title !== undefined) definition.title = options.title;
  if (options.description !== undefined) definition.description = options.description;
  if (options.outputSchema !== undefined) {
    definition.outputSchema = cloneAndFreezeJson(
      options.outputSchema as unknown as JsonValue,
    ) as unknown as ObjectSchemaShape;
  }
  if (options.annotations !== undefined) {
    definition.annotations = cloneAndFreezeJson(options.annotations) as JsonObject;
  }
  return Object.freeze(definition);
}

export function textResult(text: string): ToolResult<Record<string, never>, false>;
export function textResult<const TStructured extends object>(
  text: string,
  structuredContent: TStructured,
): ToolResult<TStructured, false>;
export function textResult(
  text: string,
  structuredContent: object = {},
): ToolResult<object, false> {
  return {
    content: [{ type: "text", text }],
    structuredContent,
    isError: false,
  };
}

export function errorResult(text: string): ToolResult<Record<string, never>, true>;
export function errorResult<const TStructured extends object>(
  text: string,
  structuredContent: TStructured,
): ToolResult<TStructured, true>;
export function errorResult(
  text: string,
  structuredContent: object = {},
): ToolResult<object, true> {
  return {
    content: [{ type: "text", text }],
    structuredContent,
    isError: true,
  };
}

export function defineTool<
  const TInput extends ObjectSchema<any>,
  const TOutput extends ObjectSchema<any> | undefined = undefined,
>(options: DefineToolOptions<TInput, TOutput>): DefinedTool<TInput, TOutput> {
  const internal: InternalDefinedTool = Object.freeze({
    [TOOL_DEFINITION]: cloneDefinition(
      options as DefineToolOptions<ObjectSchema<any>, ObjectSchema<any> | undefined>,
    ),
    [TOOL_EXECUTE]: options.execute as unknown as (
      args: object,
    ) => MaybePromise<ToolResult<object>>,
  });
  return internal as unknown as DefinedTool<TInput, TOutput>;
}

export function definePlugin<const TTools extends readonly DefinedTool[]>(
  options: DefinePluginOptions<TTools>,
): Plugin {
  const definitions: PluginToolDefinition[] = [];
  const dispatch = Object.create(null) as Record<string, InternalDefinedTool>;
  for (const tool of options.tools) {
    const internal = tool as unknown as InternalDefinedTool;
    const definition = internal[TOOL_DEFINITION];
    if (definition === undefined || internal[TOOL_EXECUTE] === undefined) {
      throw new TypeError("definePlugin received a tool not created by defineTool");
    }
    if (Object.hasOwn(dispatch, definition.name)) {
      throw new Error(`duplicate plugin tool name: ${definition.name}`);
    }
    definitions.push(definition);
    dispatch[definition.name] = internal;
  }

  const plugin: InternalPlugin = Object.freeze({
    [PLUGIN_DEFINITIONS]: Object.freeze(definitions),
    [PLUGIN_DISPATCH]: Object.freeze(dispatch),
  });
  return plugin as unknown as Plugin;
}

export function getPluginDefinitions(plugin: Plugin): readonly PluginToolDefinition[] {
  return (plugin as unknown as InternalPlugin)[PLUGIN_DEFINITIONS];
}

export function getPluginExecutor(
  plugin: Plugin,
  name: string,
): ((args: object) => MaybePromise<ToolResult<object>>) | undefined {
  return (plugin as unknown as InternalPlugin)[PLUGIN_DISPATCH][name]?.[TOOL_EXECUTE];
}
