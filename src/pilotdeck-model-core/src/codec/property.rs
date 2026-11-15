use pilotdeck_protocol::{JsObject, JsString, JsValue};

/// `Object.prototype`'s own property names under V8 (Node v22.22.0). Named so
/// a test can assert set equality in both directions instead of only probing
/// membership: adding a name here 200s a route TS answers 404 for, and no
/// membership probe would notice (#2183 评审 r2 P1).
pub const BUILTIN_PROPERTY_NAMES: [&str; 12] = [
    "__proto__",
    "constructor",
    "toString",
    "toLocaleString",
    "valueOf",
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];
pub fn builtin_property(s: &JsString) -> bool {
    BUILTIN_PROPERTY_NAMES.iter().any(|v| s.equals_str(v))
}

static NULL: JsValue = JsValue::Null;

/// Own function members of `Array.prototype` (Node 22). `length` is handled on
/// the array itself and `__proto__` is inherited from `Object.prototype`.
const ARRAY_PROTOTYPE_METHODS: &[&str] = &[
    "constructor",
    "at",
    "concat",
    "copyWithin",
    "fill",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "lastIndexOf",
    "pop",
    "push",
    "reverse",
    "shift",
    "unshift",
    "slice",
    "sort",
    "splice",
    "includes",
    "indexOf",
    "join",
    "keys",
    "entries",
    "values",
    "forEach",
    "filter",
    "flat",
    "flatMap",
    "map",
    "every",
    "some",
    "reduce",
    "reduceRight",
    "toReversed",
    "toSorted",
    "toSpliced",
    "with",
    "toLocaleString",
    "toString",
];

/// Which prototype supplied an inherited function. `Object.prototype` and
/// `Array.prototype` expose distinct functions under the same names, and
/// `Object.is` distinguishes them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuiltinPrototype {
    Object,
    Array,
}

/// A property read result. The wire model stores JSON values but not the
/// inherited function members of `Object.prototype`/`Array.prototype`, nor
/// `Object.prototype` itself; those are represented here so `diff_values` can
/// apply the baseline's `Object.is` rules to them. Shared with the runtime's
/// provider-count lookup, which needs the same JavaScript index semantics.
#[derive(Clone, Copy)]
pub enum Operand<'a> {
    Value(&'a JsValue),
    Number(f64),
    Function(BuiltinPrototype),
    ObjectPrototype,
}

impl<'a> Operand<'a> {
    pub fn as_array(self) -> Option<&'a [JsValue]> {
        match self {
            Self::Value(JsValue::Array(items)) => Some(items),
            _ => None,
        }
    }
    pub fn entries(self) -> &'a [(JsString, JsValue)] {
        match self {
            Self::Value(JsValue::Object(object)) => object.entries(),
            _ => &[],
        }
    }
    pub fn is_diffable_object(self) -> bool {
        matches!(
            self,
            Self::Value(JsValue::Object(_)) | Self::ObjectPrototype
        )
    }
}

/// Property read following the baseline's JavaScript lookup: own entries, the
/// explicit prototype chain, then the builtin members of `Object.prototype`
/// (and `Array.prototype` when an array appears in the chain). A `None`
/// prototype means the implicit `Object.prototype`; an explicit `Null`
/// prototype (for example `Object.create(null)`) has no builtin fallback.
/// Shared with the runtime's provider-count lookup rather than duplicated.
pub fn get_property<'a>(operand: Operand<'a>, key: &JsString) -> Option<Operand<'a>> {
    match operand {
        Operand::Value(JsValue::Object(object)) => object_property(object, key),
        Operand::ObjectPrototype => object_prototype_property(key),
        _ => None,
    }
}

/// Materialized property read for callers that need an owned value (config
/// source merging). Inherited function members cannot be merged as records, so
/// they map to `undefined` exactly like a miss at that call site.
pub fn property_value(receiver: &JsObject, key: &JsString) -> JsValue {
    match object_property(receiver, key) {
        Some(Operand::Value(value)) => value.clone(),
        Some(Operand::Number(number)) => JsValue::Number(number),
        Some(Operand::Function(_) | Operand::ObjectPrototype) | None => JsValue::Undefined,
    }
}

pub fn object_property<'a>(receiver: &'a JsObject, key: &JsString) -> Option<Operand<'a>> {
    let mut current = Some(receiver);
    let mut array_prototype = false;
    while let Some(object) = current {
        if let Some(value) = object.get_key(key) {
            return Some(Operand::Value(value));
        }
        match object.prototype() {
            None => break,
            Some(JsValue::Object(prototype)) => current = Some(prototype),
            Some(JsValue::Array(items)) => {
                if key.equals_str("length") {
                    return Some(Operand::Number(items.len() as f64));
                }
                // ECMAScript array indices stop at 2^32-2, the same bound as
                // the shared key rule; larger keys are ordinary properties.
                if let Some(index) = key.array_index()
                    && let Some(value) = items.get(index as usize)
                {
                    return Some(Operand::Value(value));
                }
                array_prototype = true;
                current = None;
            }
            Some(_) => return None,
        }
    }
    if key.equals_str("__proto__") {
        return Some(match receiver.prototype() {
            Some(prototype) => Operand::Value(prototype),
            None => Operand::ObjectPrototype,
        });
    }
    if array_prototype && is_array_prototype_method(key) {
        return Some(Operand::Function(BuiltinPrototype::Array));
    }
    if is_object_prototype_function(key) {
        return Some(Operand::Function(BuiltinPrototype::Object));
    }
    None
}

/// `Object.prototype` has no enumerable own keys; reading `__proto__` yields
/// its own null prototype, and the named members are functions.
pub fn object_prototype_property<'a>(key: &JsString) -> Option<Operand<'a>> {
    if key.equals_str("__proto__") {
        return Some(Operand::Value(&NULL));
    }
    is_object_prototype_function(key).then_some(Operand::Function(BuiltinPrototype::Object))
}

fn is_object_prototype_function(key: &JsString) -> bool {
    !key.equals_str("__proto__") && builtin_property(key)
}

pub fn is_array_prototype_method(key: &JsString) -> bool {
    ARRAY_PROTOTYPE_METHODS
        .iter()
        .any(|name| key.equals_str(name))
}
