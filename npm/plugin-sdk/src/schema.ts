import type {
  AnySchema,
  ArraySchemaShape,
  BooleanSchemaShape,
  InferSchema,
  JsonObject,
  JsonValue,
  NullSchemaShape,
  NumberSchemaShape,
  ObjectSchema,
  ObjectSchemaShape,
  OptionalSchema,
  PropertyMap,
  PropertySchema,
  Schema,
  SchemaNode,
  StringSchemaShape,
} from "./types.js";

type CommonOptions<TValue extends JsonValue> = {
  readonly title?: string;
  readonly description?: string;
  readonly enum?: readonly TValue[];
  readonly const?: TValue;
};

type StringOptions = CommonOptions<string> & {
  readonly minLength?: number;
  readonly maxLength?: number;
};

type NumberOptions = CommonOptions<number>;
type BooleanOptions = CommonOptions<boolean>;
type NullOptions = CommonOptions<null>;

type ArrayValue = readonly JsonValue[];
type ArrayOptions = CommonOptions<ArrayValue> & {
  readonly minItems?: number;
  readonly maxItems?: number;
};

type ObjectOptions = CommonOptions<JsonObject> & {
  readonly additionalProperties?: boolean;
};

type NarrowValue<TOptions, TFallback> = TOptions extends { readonly const: infer TValue }
  ? TValue
  : TOptions extends { readonly enum: readonly (infer TValue)[] }
    ? TValue
    : TFallback;

type OptionalKeys<TProperties extends PropertyMap> = {
  [K in keyof TProperties]-?: TProperties[K] extends OptionalSchema<AnySchema> ? K : never;
}[keyof TProperties];

type RequiredKeys<TProperties extends PropertyMap> = Exclude<
  keyof TProperties,
  OptionalKeys<TProperties>
>;

type PropertyValue<TProperty extends PropertySchema> =
  TProperty extends OptionalSchema<infer TSchema>
    ? InferSchema<TSchema>
    : TProperty extends AnySchema
      ? InferSchema<TProperty>
      : never;

type KnownObjectValue<TProperties extends PropertyMap> = {
  [K in RequiredKeys<TProperties>]: PropertyValue<TProperties[K]>;
} & {
  [K in OptionalKeys<TProperties>]?: PropertyValue<TProperties[K]>;
};

type Simplify<T> = { [K in keyof T]: T[K] };

type ObjectValue<TProperties extends PropertyMap, TOptions> = Simplify<
  KnownObjectValue<TProperties>
> &
  (TOptions extends { readonly additionalProperties: true } ? Record<string, unknown> : unknown);

type ArrayItemValue<TItem extends AnySchema> = InferSchema<TItem>[];

type StringValue<TOptions> = NarrowValue<TOptions, string>;
type NumberValue<TOptions> = NarrowValue<TOptions, number>;
type BooleanValue<TOptions> = NarrowValue<TOptions, boolean>;
type NullValue<TOptions> = NarrowValue<TOptions, null>;
type ArraySchemaValue<TItem extends AnySchema, TOptions> = NarrowValue<
  TOptions,
  ArrayItemValue<TItem>
>;
type ObjectSchemaValue<TProperties extends PropertyMap, TOptions> = NarrowValue<
  TOptions,
  ObjectValue<TProperties, TOptions>
>;

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

function applyCommonOptions(
  target: Record<string, JsonValue>,
  options: CommonOptions<JsonValue> | undefined,
): void {
  if (options?.title !== undefined) target.title = options.title;
  if (options?.description !== undefined) target.description = options.description;
  if (options?.enum !== undefined) target.enum = options.enum;
  if (options?.const !== undefined) target.const = options.const;
}

function finish<TValue, TShape extends SchemaNode>(shape: TShape): Schema<TValue, TShape> {
  return cloneAndFreezeJson(shape as unknown as JsonValue) as Schema<TValue, TShape>;
}

function string<const TOptions extends StringOptions = {}>(
  options?: TOptions,
): Schema<StringValue<TOptions>, StringSchemaShape> {
  const shape: Record<string, JsonValue> = { type: "string" };
  applyCommonOptions(shape, options);
  if (options?.minLength !== undefined) shape.minLength = options.minLength;
  if (options?.maxLength !== undefined) shape.maxLength = options.maxLength;
  return finish(shape as unknown as StringSchemaShape);
}

function number<const TOptions extends NumberOptions = {}>(
  options?: TOptions,
): Schema<NumberValue<TOptions>, NumberSchemaShape> {
  const shape: Record<string, JsonValue> = { type: "number" };
  applyCommonOptions(shape, options);
  return finish(shape as unknown as NumberSchemaShape);
}

function integer<const TOptions extends NumberOptions = {}>(
  options?: TOptions,
): Schema<NumberValue<TOptions>, NumberSchemaShape> {
  const shape: Record<string, JsonValue> = { type: "integer" };
  applyCommonOptions(shape, options);
  return finish(shape as unknown as NumberSchemaShape);
}

function boolean<const TOptions extends BooleanOptions = {}>(
  options?: TOptions,
): Schema<BooleanValue<TOptions>, BooleanSchemaShape> {
  const shape: Record<string, JsonValue> = { type: "boolean" };
  applyCommonOptions(shape, options);
  return finish(shape as unknown as BooleanSchemaShape);
}

function nullSchema<const TOptions extends NullOptions = {}>(
  options?: TOptions,
): Schema<NullValue<TOptions>, NullSchemaShape> {
  const shape: Record<string, JsonValue> = { type: "null" };
  applyCommonOptions(shape, options);
  return finish(shape as unknown as NullSchemaShape);
}

function array<const TItem extends AnySchema, const TOptions extends ArrayOptions = {}>(
  item: TItem,
  options?: TOptions,
): Schema<ArraySchemaValue<TItem, TOptions>, ArraySchemaShape> {
  const shape: Record<string, JsonValue> = {
    type: "array",
    items: item as unknown as JsonValue,
  };
  applyCommonOptions(shape, options);
  if (options?.minItems !== undefined) shape.minItems = options.minItems;
  if (options?.maxItems !== undefined) shape.maxItems = options.maxItems;
  return finish(shape as unknown as ArraySchemaShape);
}

function optional<const TSchema extends AnySchema>(schema: TSchema): OptionalSchema<TSchema> {
  return Object.freeze({ optional: true, schema });
}

function object<
  const TProperties extends PropertyMap,
  const TOptions extends ObjectOptions = {},
>(
  properties: TProperties,
  options?: TOptions,
): ObjectSchema<ObjectSchemaValue<TProperties, TOptions> & object> {
  const wireProperties = Object.create(null) as Record<string, SchemaNode>;
  const required: string[] = [];
  for (const [name, property] of Object.entries(properties)) {
    if (isOptional(property)) {
      wireProperties[name] = property.schema;
    } else {
      wireProperties[name] = property;
      required.push(name);
    }
  }

  const shape: Record<string, JsonValue> = {
    type: "object",
    properties: wireProperties as unknown as JsonValue,
    required,
    additionalProperties: options?.additionalProperties ?? false,
  };
  applyCommonOptions(shape, options);
  return finish(shape as unknown as ObjectSchemaShape);
}

function isOptional(value: PropertySchema): value is OptionalSchema<AnySchema> {
  return "optional" in value && value.optional === true;
}

export const schema = Object.freeze({
  object,
  array,
  string,
  number,
  integer,
  boolean,
  null: nullSchema,
  optional,
});
