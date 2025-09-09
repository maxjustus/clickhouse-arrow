pub(crate) mod array;
pub(crate) mod dynamic;
pub(crate) mod geo;
pub(crate) mod json;
pub(crate) mod low_cardinality;
pub(crate) mod map;
pub(crate) mod nullable;
pub(crate) mod object;
pub(crate) mod sized;
pub(crate) mod string;
pub(crate) mod tuple;
pub(crate) mod variant;

use super::low_cardinality::LOW_CARDINALITY_VERSION;
use super::*;
use crate::io::ClickHouseBytesRead;

/// Macro to read discriminator based on size
/// Used by Dynamic and JSON deserializers for variable-sized discriminators
macro_rules! read_discriminator {
    (async $reader:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => u64::from($reader.read_u8().await?),
            256..=65535 => u64::from($reader.read_u16_le().await?),
            65536..=4_294_967_295 => u64::from($reader.read_u32_le().await?),
            _ => $reader.read_u64_le().await?,
        }
    };
    (sync $reader:expr, $total_types:expr) => {
        match $total_types {
            0..=255 => u64::from($reader.get_u8()),
            256..=65535 => u64::from($reader.get_u16_le()),
            65536..=4_294_967_295 => u64::from($reader.get_u32_le()),
            _ => $reader.get_u64_le(),
        }
    };
}
pub(crate) use read_discriminator;

// Core protocol parsing
pub(crate) trait ClickHouseNativeDeserializer {
    fn deserialize_prefix_async<'a, R: ClickHouseRead>(
        &'a self,
        reader: &'a mut R,
        state: &'a mut DeserializerState,
    ) -> impl Future<Output = Result<()>> + Send + 'a;

    fn deserialize_prefix<R: ClickHouseBytesRead>(
        &self,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()>;
}

impl ClickHouseNativeDeserializer for Type {
    fn deserialize_prefix_async<'a, R: ClickHouseRead>(
        &'a self,
        reader: &'a mut R,
        state: &'a mut DeserializerState,
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        use deserialize::*;
        async move {
            match self {
                Type::Int8
                | Type::Int16
                | Type::Int32
                | Type::Int64
                | Type::Int128
                | Type::Int256
                | Type::UInt8
                | Type::UInt16
                | Type::UInt32
                | Type::UInt64
                | Type::UInt128
                | Type::UInt256
                | Type::Float32
                | Type::Float64
                | Type::Decimal32(_)
                | Type::Decimal64(_)
                | Type::Decimal128(_)
                | Type::Decimal256(_)
                | Type::Uuid
                | Type::Date
                | Type::Date32
                | Type::DateTime(_)
                | Type::DateTime64(_, _)
                | Type::Ipv4
                | Type::Ipv6
                | Type::Enum8(_)
                | Type::Enum16(_) => {
                    sized::SizedDeserializer::read_prefix(self, reader, state).await?;
                }

                Type::String
                | Type::FixedSizedString(_)
                | Type::Binary
                | Type::FixedSizedBinary(_) => {
                    string::StringDeserializer::read_prefix(self, reader, state).await?;
                }

                Type::Array(_) => {
                    array::ArrayDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Tuple(_) => {
                    tuple::TupleDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Point => geo::PointDeserializer::read_prefix(self, reader, state).await?,
                Type::Ring => geo::RingDeserializer::read_prefix(self, reader, state).await?,
                Type::Polygon => geo::PolygonDeserializer::read_prefix(self, reader, state).await?,
                Type::MultiPolygon => {
                    geo::MultiPolygonDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Nullable(_) => {
                    nullable::NullableDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Map(_, _) => map::MapDeserializer::read_prefix(self, reader, state).await?,
                Type::LowCardinality(_) => {
                    low_cardinality::LowCardinalityDeserializer::read_prefix(self, reader, state)
                        .await?;
                }
                Type::Object => {
                    object::ObjectDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Variant(_) => {
                    variant::VariantDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::Dynamic { .. } => {
                    dynamic::DynamicDeserializer::read_prefix(self, reader, state).await?;
                }
                Type::JSON { .. } => {
                    json::JsonDeserializer::read_prefix(self, reader, state).await?;
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn deserialize_prefix<R: ClickHouseBytesRead>(
        &self,
        reader: &mut R,
        state: &mut DeserializerState,
    ) -> Result<()> {
        match self {
            Type::Array(inner) | Type::Nullable(inner) => {
                inner.deserialize_prefix(reader, state)?;
            }
            Type::Tuple(inner) => {
                for inner_type in inner {
                    inner_type.deserialize_prefix(reader, state)?;
                }
            }
            Type::Map(key, value) => {
                let nested = super::map::normalize_map_type(key, value);
                nested.deserialize_prefix(reader, state)?;
            }
            Type::Point => {
                for _ in 0..2 {
                    Type::Float64.deserialize_prefix(reader, state)?;
                }
            }
            Type::LowCardinality(_) => {
                let version = reader.try_get_u64_le()?;
                if version != LOW_CARDINALITY_VERSION {
                    return Err(Error::DeserializeError(format!(
                        "LowCardinality: invalid low cardinality version: {version}"
                    )));
                }
            }
            Type::Object => {
                let _ = reader.try_get_i8()?;
            }
            Type::Variant(_) => {
                variant::VariantDeserializer::read_prefix_sync(self, reader)?;
            }
            Type::Dynamic { .. } => {
                dynamic::DynamicDeserializer::read_prefix_sync(self, reader, state)?;
            }
            Type::JSON { .. } => {
                json::JsonDeserializer::read_prefix_sync(self, reader, state)?;
            }
            _ => {}
        }
        Ok(())
    }
}

// ---
// String => Type Deserialization
// ---

// For Date32: Days from 1900-01-01 to 1970-01-01
pub(crate) const DAYS_1900_TO_1970: i32 = 25_567;

trait EnumValueType: FromStr + std::fmt::Debug {}
impl EnumValueType for i8 {}
impl EnumValueType for i16 {}

macro_rules! parse_enum_options {
    ($opt_str:expr, $num_type:ty) => {{
        fn inner_parse(input: &str) -> Result<Vec<(String, $num_type)>> {
            if !input.starts_with('(') || !input.ends_with(')') {
                return Err(Error::TypeParseError(
                    "Enum arguments must be enclosed in parentheses".to_string(),
                ));
            }

            let input = input[1..input.len() - 1].trim();
            if input.is_empty() {
                return Ok(Vec::new());
            }

            let mut options = Vec::new();
            let mut name = String::new();
            let mut value = String::new();
            let mut state = EnumParseState::ExpectQuote;
            let mut escaped = false;

            for ch in input.chars() {
                match state {
                    EnumParseState::ExpectQuote => {
                        if ch == '\'' {
                            state = EnumParseState::InName;
                        } else if !ch.is_whitespace() {
                            return Err(Error::TypeParseError(format!(
                                "Expected single quote at start of variant name, found '{}'",
                                ch
                            )));
                        }
                    }
                    EnumParseState::InName => {
                        if escaped {
                            name.push(ch);
                            escaped = false;
                        } else if ch == '\\' {
                            escaped = true;
                        } else if ch == '\'' {
                            state = EnumParseState::ExpectEqual;
                        } else {
                            name.push(ch);
                        }
                    }
                    EnumParseState::ExpectEqual => {
                        if ch == '=' {
                            state = EnumParseState::InValue;
                        } else if !ch.is_whitespace() {
                            return Err(Error::TypeParseError(format!(
                                "Expected '=' after variant name, found '{}'",
                                ch
                            )));
                        }
                    }
                    EnumParseState::InValue => {
                        if ch == ',' {
                            let parsed_value = value.parse::<$num_type>().map_err(|e| {
                                Error::TypeParseError(format!("Invalid enum value '{value}': {e}"))
                            })?;
                            options.push((name, parsed_value));
                            name = String::new();
                            value = String::new();
                            state = EnumParseState::ExpectQuote;
                        } else if !ch.is_whitespace() {
                            value.push(ch);
                        }
                    }
                }
            }

            match state {
                EnumParseState::InValue if !value.is_empty() => {
                    let parsed_value = value.parse::<$num_type>().map_err(|e| {
                        Error::TypeParseError(format!("Invalid enum value '{value}': {e}"))
                    })?;
                    options.push((name, parsed_value));
                }
                EnumParseState::ExpectQuote if !input.is_empty() => {
                    return Err(Error::TypeParseError(
                        "Expected enum variant, found end of input".to_string(),
                    ));
                }
                EnumParseState::InName | EnumParseState::ExpectEqual => {
                    return Err(Error::TypeParseError(
                        "Incomplete enum variant at end of input".to_string(),
                    ));
                }
                _ => {}
            }

            if input.ends_with(',') {
                return Err(Error::TypeParseError("Trailing comma in enum variants".to_string()));
            }

            Ok(options)
        }

        fn assert_numeric_type<T: EnumValueType>() {}
        assert_numeric_type::<$num_type>();
        inner_parse($opt_str)
    }};
}

#[derive(PartialEq)]
enum EnumParseState {
    ExpectQuote,
    InName,
    ExpectEqual,
    InValue,
}

type JsonParameters = (
    Option<u32>,                 // max_dynamic_paths
    Option<u32>,                 // max_dynamic_types
    Vec<(String, Box<Type>)>,    // typed_paths
    Vec<String>,                 // skip_exact
    Vec<String>,                 // skip_regex
);

fn parse_json_parameters(args: Vec<&str>) -> Result<JsonParameters> {
    let mut max_dynamic_paths = None;
    let mut max_dynamic_types = None;
    let mut typed_paths = Vec::new();
    let mut skip_exact = Vec::new();
    let mut skip_regex = Vec::new();

    for arg in args {
        let arg = arg.trim();
        if let Some(value_str) = arg.strip_prefix("max_dynamic_paths=") {
            let value: u32 = value_str.parse().map_err(|_| {
                Error::TypeParseError(format!("Invalid max_dynamic_paths value: '{value_str}'"))
            })?;
            max_dynamic_paths = Some(value);
        } else if let Some(value_str) = arg.strip_prefix("max_dynamic_types=") {
            let value: u32 = value_str.parse().map_err(|_| {
                Error::TypeParseError(format!("Invalid max_dynamic_types value: '{value_str}'"))
            })?;
            max_dynamic_types = Some(value);
        } else if let Some(skip_path) = arg.strip_prefix("SKIP REGEXP ") {
            // Handle regex skip paths: SKIP REGEXP 'pattern'
            let pattern = skip_path
                .trim()
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .trim_start_matches('"')
                .trim_end_matches('"');
            skip_regex.push(pattern.to_string());
        } else if let Some(skip_path) = arg.strip_prefix("SKIP ") {
            // Handle literal skip paths: SKIP field_name
            let field = skip_path
                .trim()
                .trim_start_matches('`')
                .trim_end_matches('`')
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .trim_start_matches('"')
                .trim_end_matches('"');
            skip_exact.push(field.to_string());
        } else if arg.contains(' ') && !arg.starts_with("max_") && !arg.starts_with("SKIP") {
            // Handle typed paths: Name String, Age Int64, etc.
            let parts: Vec<&str> = arg.splitn(2, ' ').collect();
            if parts.len() == 2 {
                let path = parts[0].trim().trim_start_matches('`').trim_end_matches('`');
                let type_str = parts[1].trim();
                if let Ok(parsed_type) = Type::from_str(type_str) {
                    typed_paths.push((path.to_string(), Box::new(parsed_type)));
                } else {
                    // If we can't parse the type, silently ignore for
                    // compatibility
                    // This maintains backward compatibility with unknown types
                }
            }
        } else {
            // Silently ignore unrecognized parameters for forward compatibility
        }
    }

    Ok((max_dynamic_paths, max_dynamic_types, typed_paths, skip_exact, skip_regex))
}

fn parse_dynamic_parameters(args: Vec<&str>) -> Result<Option<u32>> {
    let mut max_types = None;

    for arg in args {
        let arg = arg.trim();
        if let Some(value_str) = arg.strip_prefix("max_types=") {
            let value: u32 = value_str.parse().map_err(|_| {
                Error::TypeParseError(format!("Invalid max_types value: '{value_str}'"))
            })?;
            max_types = Some(value);
        } else {
            return Err(Error::TypeParseError(format!(
                "Unknown Dynamic parameter: '{arg}'. Valid parameter is: max_types"
            )));
        }
    }

    Ok(max_types)
}

impl FromStr for Type {
    type Err = Error;

    #[expect(clippy::too_many_lines)]
    fn from_str(s: &str) -> Result<Self> {
        let (ident, following) = eat_identifier(s);

        if ident.is_empty() {
            return Err(Error::TypeParseError(format!("invalid empty identifier for type: '{s}'")));
        }

        let following = following.trim();
        if !following.is_empty() {
            return Ok(match ident {
                "Object" => {
                    // Accept Object with optional arguments like Object('json') and ignore params
                    let _args = parse_variable_args(following)?; // validate parens
                    Type::Object
                }
                "Decimal" => {
                    let (args, count) = parse_fixed_args::<2>(following)?;
                    if count != 2 {
                        return Err(Error::TypeParseError(format!(
                            "Decimal expects 2 args, got {count}: {args:?}"
                        )));
                    }
                    let p: usize = parse_precision(args[0])?;
                    let s: usize = parse_scale(args[1])?;
                    if s == 0
                        || (p <= 9 && s > 9)
                        || (p <= 18 && s > 18)
                        || (p <= 38 && s > 38)
                        || (p <= 76 && s > 76)
                    {
                        return Err(Error::TypeParseError(format!(
                            "Invalid scale {s} for precision {p}"
                        )));
                    }
                    if p <= 9 {
                        Type::Decimal32(s)
                    } else if p <= 18 {
                        Type::Decimal64(s)
                    } else if p <= 38 {
                        Type::Decimal128(s)
                    } else if p <= 76 {
                        Type::Decimal256(s)
                    } else {
                        return Err(Error::TypeParseError(
                            "bad decimal spec, cannot exceed 76 precision".to_string(),
                        ));
                    }
                }
                "Decimal32" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "bad arg count for Decimal32, expected 1 and got {count}: {args:?}"
                        )));
                    }
                    let s: usize = parse_scale(args[0])?;
                    if s == 0 || s > 9 {
                        return Err(Error::TypeParseError(format!(
                            "Invalid scale {s} for Decimal32, must be 1..=9"
                        )));
                    }
                    Type::Decimal32(s)
                }
                "Decimal64" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "bad arg count for Decimal64, expected 1 and got {count}: {args:?}"
                        )));
                    }
                    let s: usize = parse_scale(args[0])?;
                    if s == 0 || s > 18 {
                        return Err(Error::TypeParseError(format!(
                            "Invalid scale {s} for Decimal64, must be 1..=18"
                        )));
                    }
                    Type::Decimal64(s)
                }
                "Decimal128" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "bad arg count for Decimal128, expected 1 and got {count}: {args:?}"
                        )));
                    }
                    let s: usize = parse_scale(args[0])?;
                    if s == 0 || s > 38 {
                        return Err(Error::TypeParseError(format!(
                            "Invalid scale {s} for Decimal128, must be 1..=38"
                        )));
                    }
                    Type::Decimal128(s)
                }
                "Decimal256" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "bad arg count for Decimal256, expected 1 and got {count}: {args:?}"
                        )));
                    }
                    let s: usize = parse_scale(args[0])?;
                    if s == 0 || s > 76 {
                        return Err(Error::TypeParseError(format!(
                            "Invalid scale {s} for Decimal256, must be 1..=76"
                        )));
                    }
                    Type::Decimal256(s)
                }
                "FixedString" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "bad arg count for FixedString, expected 1 and got {count}: {args:?}"
                        )));
                    }
                    let s: usize = parse_scale(args[0])?;
                    if s == 0 {
                        return Err(Error::TypeParseError(
                            "FixedString size must be greater than 0".to_string(),
                        ));
                    }
                    Type::FixedSizedString(s)
                }
                "DateTime" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count > 1 {
                        return Err(Error::TypeParseError(format!(
                            "DateTime expects 0 or 1 arg: {args:?}"
                        )));
                    }
                    if count == 0 {
                        Type::DateTime(chrono_tz::UTC)
                    } else {
                        let tz_str = args[0];
                        if !tz_str.starts_with('\'') || !tz_str.ends_with('\'') {
                            return Err(Error::TypeParseError(format!(
                                "DateTime timezone must be quoted: '{tz_str}'"
                            )));
                        }
                        let tz = tz_str[1..tz_str.len() - 1].parse().map_err(|e| {
                            Error::TypeParseError(format!(
                                "failed to parse timezone '{tz_str}': {e}"
                            ))
                        })?;
                        Type::DateTime(tz)
                    }
                }
                "DateTime64" => {
                    let (args, count) = parse_fixed_args::<2>(following)?;
                    if !(1..=2).contains(&count) {
                        return Err(Error::TypeParseError(format!(
                            "DateTime64 expects 1 or 2 args, got {count}: {args:?}"
                        )));
                    }
                    let precision = parse_precision(args[0])?;
                    let tz = if count == 2 {
                        let tz_str = args[1];
                        if !tz_str.starts_with('\'') || !tz_str.ends_with('\'') {
                            return Err(Error::TypeParseError(format!(
                                "DateTime64 timezone must be quoted: '{tz_str}'"
                            )));
                        }
                        tz_str[1..tz_str.len() - 1].parse().map_err(|e| {
                            Error::TypeParseError(format!(
                                "failed to parse timezone '{tz_str}': {e}"
                            ))
                        })?
                    } else {
                        chrono_tz::UTC
                    };
                    Type::DateTime64(precision, tz)
                }
                "Enum8" => Type::Enum8(parse_enum_options!(following, i8)?),
                "Enum16" => Type::Enum16(parse_enum_options!(following, i16)?),
                "LowCardinality" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "LowCardinality expected 1 arg and got {count}: {args:?}"
                        )));
                    }
                    Type::LowCardinality(Box::new(Type::from_str(args[0])?))
                }
                "Array" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "Array expected 1 arg and got {count}: {args:?}"
                        )));
                    }
                    Type::Array(Box::new(Type::from_str(args[0])?))
                }
                "Tuple" => {
                    // Support both positional and named tuple fields, e.g.:
                    //   Tuple(Int8, String)
                    //   Tuple(id Int8, name String)
                    let args = parse_variable_args(following)?;
                    let mut inner: Vec<Type> = Vec::with_capacity(args.len());
                    for arg in args {
                        // Try plain positional type first
                        match Type::from_str(arg) {
                            Ok(t) => inner.push(t),
                            Err(_) => {
                                // Fallback: accept named field form "name Type"
                                let (ident, rest) = eat_identifier(arg);
                                let rest = rest.trim();
                                if !ident.is_empty() && !rest.is_empty() {
                                    inner.push(Type::from_str(rest)?);
                                } else {
                                    return Err(Error::TypeParseError(format!(
                                        "invalid type with arguments: '{arg}' (ident = {ident})"
                                    )));
                                }
                            }
                        }
                    }
                    Type::Tuple(inner)
                }
                "Nullable" => {
                    let (args, count) = parse_fixed_args::<1>(following)?;
                    if count != 1 {
                        return Err(Error::TypeParseError(format!(
                            "Nullable expects 1 arg: {args:?}"
                        )));
                    }
                    Type::Nullable(Box::new(Type::from_str(args[0])?))
                }
                "Map" => {
                    let (args, count) = parse_fixed_args::<2>(following)?;
                    if count != 2 {
                        return Err(Error::TypeParseError(format!(
                            "Map expects 2 args, got {count}: {args:?}"
                        )));
                    }
                    Type::Map(
                        Box::new(Type::from_str(args[0])?),
                        Box::new(Type::from_str(args[1])?),
                    )
                }
                "Variant" => {
                    let args = parse_variable_args(following)?;
                    if args.is_empty() {
                        return Err(Error::TypeParseError(
                            "Variant expects at least one type argument".to_string(),
                        ));
                    }
                    // Preserve declared order for Variant inner types. Canonical ordering for
                    // discriminators is handled at runtime by DiscriminatorMap.
                    let inner: Vec<Type> =
                        args.into_iter().map(Type::from_str).collect::<Result<_, _>>()?;
                    Type::Variant(inner)
                }
                "Dynamic" => {
                    let args = parse_variable_args(following)?;
                    let max_types = parse_dynamic_parameters(args)?;
                    Type::Dynamic { max_types }
                }
                "JSON" => {
                    let args = parse_variable_args(following)?;
                    let (max_dynamic_paths, max_dynamic_types, typed_paths, skip_exact, skip_regex) =
                        parse_json_parameters(args)?;
                    Type::JSON { max_dynamic_paths, max_dynamic_types, typed_paths, skip_exact, skip_regex }
                }
                // Unsupported
                "Nested" => {
                    return Err(Error::TypeParseError("unsupported Nested type".to_string()));
                }
                id => {
                    return Err(Error::TypeParseError(format!(
                        "invalid type with arguments: '{ident}' (ident = {id})"
                    )));
                }
            });
        }
        Ok(match ident {
            "Int8" => Type::Int8,
            "Int16" => Type::Int16,
            "Int32" => Type::Int32,
            "Int64" => Type::Int64,
            "Int128" => Type::Int128,
            "Int256" => Type::Int256,
            "Bool" | "UInt8" => Type::UInt8,
            "UInt16" => Type::UInt16,
            "UInt32" => Type::UInt32,
            "UInt64" => Type::UInt64,
            "UInt128" => Type::UInt128,
            "UInt256" => Type::UInt256,
            "Float32" => Type::Float32,
            "Float64" => Type::Float64,
            "String" => Type::String,
            "UUID" | "Uuid" | "uuid" => Type::Uuid,
            "Date" => Type::Date,
            "Date32" => Type::Date32,
            // TODO: This is duplicated above. Verify if this is needed, for example if ClickHouse
            // ever sends `DateTime` without tz.
            "DateTime" => Type::DateTime(chrono_tz::UTC),
            "IPv4" => Type::Ipv4,
            "IPv6" => Type::Ipv6,
            "Point" => Type::Point,
            "Ring" => Type::Ring,
            "Polygon" => Type::Polygon,
            "MultiPolygon" => Type::MultiPolygon,
            "Object" | "Json" | "OBJECT" => Type::Object,
            "JSON" => {
                if following.is_empty() {
                    Type::JSON {
                        max_dynamic_paths: None,
                        max_dynamic_types: None,
                        typed_paths:       Vec::new(),
                        skip_exact:        Vec::new(),
                        skip_regex:        Vec::new(),
                    }
                } else {
                    let args = parse_variable_args(following)?;
                    let (max_dynamic_paths, max_dynamic_types, typed_paths, skip_exact, skip_regex) =
                        parse_json_parameters(args)?;
                    Type::JSON { max_dynamic_paths, max_dynamic_types, typed_paths, skip_exact, skip_regex }
                }
            }
            "Dynamic" => {
                if following.is_empty() {
                    Type::Dynamic { max_types: None }
                } else {
                    let args = parse_variable_args(following)?;
                    let max_types = parse_dynamic_parameters(args)?;
                    Type::Dynamic { max_types }
                }
            }
            _ => {
                return Err(Error::TypeParseError(format!("invalid type name: '{ident}'")));
            }
        })
    }
}

// Assumed complete identifier normalization and type resolution from clickhouse
fn eat_identifier(input: &str) -> (&str, &str) {
    for (i, c) in input.char_indices() {
        if c.is_alphabetic() || c == '_' || c == '$' || (i > 0 && c.is_numeric()) {
            continue;
        }
        return (&input[..i], &input[i..]);
    }
    (input, "")
}

/// Parse arguments into a fixed-size array for types with a known number of args
fn parse_fixed_args<const N: usize>(input: &str) -> Result<([&str; N], usize)> {
    let mut iter = parse_args_iter(input)?;
    let mut out = [""; N];
    let mut count = 0;

    // Take up to N items
    for (i, arg_result) in iter.by_ref().take(N).enumerate() {
        out[i] = arg_result?;
        count += 1;
    }

    // Check for excess arguments
    if iter.next().is_some() {
        return Err(Error::TypeParseError("too many arguments".to_string()));
    }
    Ok((out, count))
}

/// Parse arguments into a Vec for types with variable numbers of args
fn parse_variable_args(input: &str) -> Result<Vec<&str>> { parse_args_iter(input)?.collect() }

fn parse_scale(from: &str) -> Result<usize> {
    from.parse().map_err(|_| Error::TypeParseError("couldn't parse scale".to_string()))
}

fn parse_precision(from: &str) -> Result<usize> {
    from.parse().map_err(|_| Error::TypeParseError("could not parse precision".to_string()))
}

/// Core iterator for parsing comma-separated arguments within parentheses
fn parse_args_iter(input: &str) -> Result<impl Iterator<Item = Result<&str, Error>>> {
    if !input.starts_with('(') || !input.ends_with(')') {
        return Err(Error::TypeParseError("Malformed arguments to type".to_string()));
    }
    let input = input[1..input.len() - 1].trim();
    if input.ends_with(',') {
        return Err(Error::TypeParseError("Trailing comma in argument list".to_string()));
    }

    Ok(ArgsIterator { input, last_start: 0, in_parens: 0, in_quotes: false, done: false })
}

struct ArgsIterator<'a> {
    input:      &'a str,
    last_start: usize,
    in_parens:  usize,
    in_quotes:  bool,
    done:       bool,
}

impl<'a> Iterator for ArgsIterator<'a> {
    type Item = Result<&'a str, Error>;

    #[allow(unused_assignments)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        let start = self.last_start;
        let mut i = start;
        let chars = self.input[start..].char_indices();
        let mut escaped = false;

        for (offset, c) in chars {
            i = start + offset;
            if self.in_quotes {
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == '\'' && !escaped {
                    self.in_quotes = false;
                }
                escaped = false;
                continue;
            }
            match c {
                '\'' if !escaped => {
                    self.in_quotes = true;
                }
                '(' => self.in_parens += 1,
                ')' => self.in_parens -= 1,
                ',' if self.in_parens == 0 => {
                    let slice = self.input[self.last_start..i].trim();
                    if slice.is_empty() {
                        return Some(Err(Error::TypeParseError(
                            "Empty argument in list".to_string(),
                        )));
                    }
                    self.last_start = i + 1;
                    return Some(Ok(slice));
                }
                _ => {}
            }
            escaped = false;
        }

        if self.in_parens != 0 {
            self.done = true;
            return Some(Err(Error::TypeParseError("Mismatched parentheses".to_string())));
        }
        if self.last_start <= self.input.len() {
            let slice = self.input[self.last_start..].trim();
            if slice.is_empty() {
                self.done = true;
                return None; // Allow empty input after last comma
            }
            if slice == "," {
                self.done = true;
                return Some(Err(Error::TypeParseError(
                    "Trailing comma in argument list".to_string(),
                )));
            }
            self.done = true;
            return Some(Ok(slice));
        }

        self.done = true;
        None
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    /// Tests `eat_identifier` for splitting type names and arguments.
    #[test]
    fn test_eat_identifier() {
        assert_eq!(eat_identifier("Int8"), ("Int8", ""));
        assert_eq!(eat_identifier("Enum8('a'=1)"), ("Enum8", "('a'=1)"));
        assert_eq!(eat_identifier("DateTime('UTC')"), ("DateTime", "('UTC')"));
        assert_eq!(eat_identifier("Map(String,Int32)"), ("Map", "(String,Int32)"));
        assert_eq!(eat_identifier(""), ("", ""));
        assert_eq!(eat_identifier("Invalid Type"), ("Invalid", " Type"));
    }

    /// Tests `parse_fixed_args` for fixed-size argument lists.
    #[test]
    fn test_parse_fixed_args() {
        let (args, count) = parse_fixed_args::<2>("(UInt32, String)").unwrap();
        assert_eq!(count, 2);
        assert_eq!(args[..count], ["UInt32", "String"]);

        let (args, count) = parse_fixed_args::<1>("(String)").unwrap();
        assert_eq!(count, 1);
        assert_eq!(args[..count], ["String"]);

        let (args, count) = parse_fixed_args::<2>("(3, 'UTC')").unwrap();
        assert_eq!(count, 2);
        assert_eq!(args[..count], ["3", "'UTC'"]);

        assert!(parse_fixed_args::<1>("(UInt32, String)").is_err()); // Too many args
        assert!(parse_fixed_args::<1>("(()").is_err()); // Mismatched parens
        assert!(parse_fixed_args::<1>("(String,)").is_err()); // Trailing comma
    }

    /// Tests `parse_variable_args` for variable-size argument lists.
    #[test]
    fn test_parse_variable_args() {
        let args = parse_variable_args("(Int8, String, Float64)").unwrap();
        assert_eq!(args, vec!["Int8", "String", "Float64"]);

        let args = parse_variable_args("(3, 'UTC', 'extra')").unwrap();
        assert_eq!(args, vec!["3", "'UTC'", "'extra'"]);

        let args = parse_variable_args("(())").unwrap();
        assert_eq!(args, vec!["()"]);

        let args = parse_variable_args("()").unwrap();
        assert_eq!(args, Vec::<&str>::new());

        assert!(parse_variable_args("(()").is_err()); // Mismatched parens
        assert!(parse_variable_args("(String,)").is_err()); // Trailing comma
    }

    /// Tests `Type::from_str` for primitive types.
    #[test]
    fn test_from_str_primitives() {
        assert_eq!(Type::from_str("Int8").unwrap(), Type::Int8);
        assert_eq!(Type::from_str("UInt8").unwrap(), Type::UInt8);
        assert_eq!(Type::from_str("Bool").unwrap(), Type::UInt8); // Bool alias
        assert_eq!(Type::from_str("Float64").unwrap(), Type::Float64);
        assert_eq!(Type::from_str("String").unwrap(), Type::String);
        assert_eq!(Type::from_str("UUID").unwrap(), Type::Uuid);
        assert_eq!(Type::from_str("Date").unwrap(), Type::Date);
        assert_eq!(Type::from_str("IPv4").unwrap(), Type::Ipv4);
        assert_eq!(Type::from_str("IPv6").unwrap(), Type::Ipv6);
    }

    /// Tests `Type::from_str` for decimal types.
    #[test]
    fn test_from_str_decimals() {
        assert_eq!(Type::from_str("Decimal32(2)").unwrap(), Type::Decimal32(2));
        assert_eq!(Type::from_str("Decimal64(4)").unwrap(), Type::Decimal64(4));
        assert_eq!(Type::from_str("Decimal128(6)").unwrap(), Type::Decimal128(6));
        assert_eq!(Type::from_str("Decimal256(8)").unwrap(), Type::Decimal256(8));
        assert_eq!(Type::from_str("Decimal(9, 2)").unwrap(), Type::Decimal32(2));
        assert_eq!(Type::from_str("Decimal(18, 4)").unwrap(), Type::Decimal64(4));
        assert_eq!(Type::from_str("Decimal(38, 6)").unwrap(), Type::Decimal128(6));
        assert_eq!(Type::from_str("Decimal(76, 8)").unwrap(), Type::Decimal256(8));

        assert!(Type::from_str("Decimal32(0)").is_err()); // Invalid scale
        assert!(Type::from_str("Decimal(77, 8)").is_err()); // Precision too large
        assert!(Type::from_str("Decimal(9)").is_err()); // Missing scale
    }

    /// Tests `Type::from_str` for string and binary types.
    #[test]
    fn test_from_str_strings() {
        assert_eq!(Type::from_str("String").unwrap(), Type::String);
        assert_eq!(Type::from_str("FixedString(4)").unwrap(), Type::FixedSizedString(4));
        assert!(Type::from_str("FixedString(0)").is_err()); // Invalid size
        assert!(Type::from_str("FixedString(a)").is_err()); // Invalid size
    }

    /// Tests `Type::from_str` for date and time types.
    #[test]
    fn test_from_str_datetime() {
        assert_eq!(Type::from_str("DateTime").unwrap(), Type::DateTime(chrono_tz::UTC));
        assert_eq!(Type::from_str("DateTime('UTC')").unwrap(), Type::DateTime(chrono_tz::UTC));
        assert_eq!(
            Type::from_str("DateTime('America/New_York')").unwrap(),
            Type::DateTime(chrono_tz::America::New_York)
        );
        assert!(Type::from_str("DateTime('UTC', 'extra')").is_err()); // Too many args
        assert!(Type::from_str("DateTime(UTC)").is_err()); // Unquoted timezone

        assert_eq!(Type::from_str("DateTime64(3)").unwrap(), Type::DateTime64(3, chrono_tz::UTC));
        assert_eq!(
            Type::from_str("DateTime64(6, 'UTC')").unwrap(),
            Type::DateTime64(6, chrono_tz::UTC)
        );
        assert_eq!(
            Type::from_str("DateTime64(3, 'America/New_York')").unwrap(),
            Type::DateTime64(3, chrono_tz::America::New_York)
        );
        assert!(Type::from_str("DateTime64()").is_err()); // Too few args
        assert!(Type::from_str("DateTime64(3, 'UTC', 'extra')").is_err()); // Too many args
        assert!(Type::from_str("DateTime64(3, UTC)").is_err()); // Unquoted timezone
    }

    /// Tests `Type::from_str` for Enum8 with explicit indices.
    #[test]
    fn test_from_str_enum8_explicit() {
        let enum8 = Type::from_str("Enum8('active' = 1, 'inactive' = 2)").unwrap();
        assert_eq!(enum8, Type::Enum8(vec![("active".into(), 1), ("inactive".into(), 2)]));

        let single = Type::from_str("Enum8('test' = -1)").unwrap();
        assert_eq!(single, Type::Enum8(vec![("test".into(), -1)]));

        let negative = Type::from_str("Enum8('neg' = -128, 'zero' = 0)").unwrap();
        assert_eq!(negative, Type::Enum8(vec![("neg".into(), -128), ("zero".into(), 0)]));
    }

    /// Tests `Type::from_str` for Enum8 with empty variants.
    #[test]
    fn test_from_str_enum8_empty() {
        let empty = Type::from_str("Enum8()").unwrap();
        assert_eq!(empty, Type::Enum8(vec![]));
    }

    /// Tests `Type::from_str` for Enum16 with explicit indices.
    #[test]
    fn test_from_str_enum16_explicit() {
        let enum16 = Type::from_str("Enum16('high' = 1000, 'low' = -1000)").unwrap();
        assert_eq!(enum16, Type::Enum16(vec![("high".into(), 1000), ("low".into(), -1000)]));

        let single = Type::from_str("Enum16('test' = 0)").unwrap();
        assert_eq!(single, Type::Enum16(vec![("test".into(), 0)]));
    }

    /// Tests `Type::from_str` error cases for Enum8.
    #[test]
    fn test_from_str_enum8_errors() {
        assert!(Type::from_str("Enum8('a' = 1, 2)").is_err()); // Lone value
        assert!(Type::from_str("Enum8('a' = x)").is_err()); // Invalid value
        assert!(Type::from_str("Enum8(a = 1)").is_err()); // Unquoted name
        assert!(Type::from_str("Enum8('a' = 1, )").is_err()); // Trailing comma
        assert!(Type::from_str("Enum8('a' = 1").is_err()); // Unclosed paren
    }

    /// Tests `Type::from_str` error cases for Enum16.
    #[test]
    fn test_from_str_enum16_errors() {
        assert!(Type::from_str("Enum16('a' = 1, 2)").is_err()); // Lone value
        assert!(Type::from_str("Enum16('a' = x)").is_err()); // Invalid value
        assert!(Type::from_str("Enum16(a = 1)").is_err()); // Unquoted name
        assert!(Type::from_str("Enum16('a' = 1, )").is_err()); // Trailing comma
        assert!(Type::from_str("Enum16('a' = 1").is_err()); // Unclosed paren
    }

    /// Tests `Type::from_str` for complex types.
    #[test]
    fn test_from_str_complex_types() {
        assert_eq!(
            Type::from_str("LowCardinality(String)").unwrap(),
            Type::LowCardinality(Box::new(Type::String))
        );
        assert_eq!(Type::from_str("Array(Int32)").unwrap(), Type::Array(Box::new(Type::Int32)));
        assert_eq!(
            Type::from_str("Tuple(Int32, String)").unwrap(),
            Type::Tuple(vec![Type::Int32, Type::String])
        );
        assert_eq!(
            Type::from_str("Nullable(Int32)").unwrap(),
            Type::Nullable(Box::new(Type::Int32))
        );
        assert_eq!(
            Type::from_str("Map(String, Int32)").unwrap(),
            Type::Map(Box::new(Type::String), Box::new(Type::Int32))
        );
        assert_eq!(Type::from_str("JSON").unwrap(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        });
        assert_eq!(Type::from_str("Object").unwrap(), Type::Object);
        assert_eq!(Type::from_str("Json").unwrap(), Type::Object);

        assert!(Type::from_str("LowCardinality()").is_err()); // Missing arg
        assert!(Type::from_str("Array(Int32, String)").is_err()); // Too many args
        assert!(Type::from_str("Map(String)").is_err()); // Missing value type
    }

    /// Tests round-trip `to_string` and `from_str` for all types.
    #[test]
    fn test_round_trip_type_strings() {
        let special_types = vec![
            (Type::Binary, Type::String),
            (Type::FixedSizedBinary(8), Type::FixedSizedString(8)),
        ];

        let types = vec![
            Type::Int8,
            Type::UInt8,
            Type::Float64,
            Type::String,
            Type::FixedSizedString(4),
            Type::Uuid,
            Type::Date,
            Type::Date32,
            Type::DateTime(Tz::UTC),
            Type::DateTime64(3, Tz::America__New_York),
            Type::Ipv4,
            Type::Ipv6,
            Type::Decimal32(2),
            Type::Enum8(vec![("active".into(), 1), ("inactive".into(), 2)]),
            Type::Enum16(vec![("high".into(), 1000)]),
            Type::LowCardinality(Box::new(Type::String)),
            Type::Array(Box::new(Type::Int32)),
            Type::Tuple(vec![Type::Int32, Type::String]),
            Type::Nullable(Box::new(Type::Int32)),
            Type::Map(Box::new(Type::String), Box::new(Type::Int32)),
            Type::Object,
        ];

        for ty in types {
            let type_str = ty.to_string();
            let parsed = Type::from_str(&type_str)
                .unwrap_or_else(|e| panic!("Failed to parse '{type_str}' for type {ty:?}: {e}"));
            assert_eq!(
                parsed, ty,
                "Round-trip failed for type {ty:?}: expected {ty}, got {parsed}"
            );
        }

        for (ty, mapped_ty) in special_types {
            let type_str = ty.to_string();
            let parsed = Type::from_str(&type_str)
                .unwrap_or_else(|e| panic!("Failed to parse '{type_str}' for type {ty:?}: {e}"));
            assert_eq!(
                parsed, mapped_ty,
                "Round-trip failed for type {ty:?}: expected {mapped_ty}, got {parsed}"
            );
        }
    }

    /// Tests error cases for general type parsing.
    #[test]
    fn test_from_str_general_errors() {
        assert!(Type::from_str("").is_err()); // Empty input
        assert!(Type::from_str("InvalidType").is_err()); // Unknown type
        assert!(Type::from_str("Nested(String)").is_err()); // Unsupported Nested
        assert!(Type::from_str("Int8(").is_err()); // Unclosed paren
        assert!(Type::from_str("Tuple(String,)").is_err()); // Trailing comma
    }

    /// Tests parsing of parameterized Dynamic and JSON types.
    #[test]
    fn test_from_str_parameterized_types() {
        // Test Dynamic with parameters
        assert_eq!(Type::from_str("Dynamic").unwrap(), Type::Dynamic { max_types: None });
        assert_eq!(Type::from_str("Dynamic(max_types=16)").unwrap(), Type::Dynamic {
            max_types: Some(16),
        });
        assert_eq!(Type::from_str("Dynamic(max_types=100)").unwrap(), Type::Dynamic {
            max_types: Some(100),
        });

        // Test JSON with parameters
        assert_eq!(Type::from_str("JSON").unwrap(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        });
        assert_eq!(Type::from_str("JSON(max_dynamic_paths=16)").unwrap(), Type::JSON {
            max_dynamic_paths: Some(16),
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        });
        assert_eq!(Type::from_str("JSON(max_dynamic_types=64)").unwrap(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: Some(64),
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![],
        });
        assert_eq!(
            Type::from_str("JSON(max_dynamic_paths=100, max_dynamic_types=32)").unwrap(),
            Type::JSON {
                max_dynamic_paths: Some(100),
                max_dynamic_types: Some(32),
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            }
        );
        assert_eq!(
            Type::from_str("JSON(max_dynamic_types=32, max_dynamic_paths=100)").unwrap(),
            Type::JSON {
                max_dynamic_paths: Some(100),
                max_dynamic_types: Some(32),
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            }
        );

        // Test error cases
        assert!(Type::from_str("Dynamic(invalid_param=16)").is_err());
        assert!(Type::from_str("Dynamic(max_types=abc)").is_err());
        assert!(Type::from_str("JSON(max_dynamic_paths=abc)").is_err());
        assert!(Type::from_str("JSON(max_dynamic_types=xyz)").is_err());

        // Test typed paths parsing
        assert_eq!(
            Type::from_str("JSON(Name String, max_dynamic_paths=100)").unwrap(),
            Type::JSON {
                max_dynamic_paths: Some(100),
                max_dynamic_types: None,
                typed_paths:       vec![("Name".to_string(), Box::new(Type::String))],
                skip_exact:        vec![],
                skip_regex:        vec![],
            }
        );
        assert_eq!(Type::from_str("JSON(Name String, Age Int64)").unwrap(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![
                ("Name".to_string(), Box::new(Type::String)),
                ("Age".to_string(), Box::new(Type::Int64))
            ],
            skip_exact:        vec![],
            skip_regex:        vec![],
        });

        // Test skip paths parsing
        assert_eq!(
            Type::from_str("JSON(SKIP fake.field, max_dynamic_types=32)").unwrap(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: Some(32),
                typed_paths:       vec![],
                skip_exact:        vec!["fake.field".to_string()],
                skip_regex:        vec![],
            }
        );
        assert_eq!(Type::from_str("JSON(SKIP REGEXP '.*\\.debug')").unwrap(), Type::JSON {
            max_dynamic_paths: None,
            max_dynamic_types: None,
            typed_paths:       vec![],
            skip_exact:        vec![],
            skip_regex:        vec![".*\\.debug".to_string()],
        });

        // Test combined typed paths and skip paths
        assert_eq!(
            Type::from_str(
                "JSON(Name String, Age Int64, SKIP REGEXP '.*\\.debug', SKIP temp.field)"
            )
            .unwrap(),
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths:       vec![
                    ("Name".to_string(), Box::new(Type::String)),
                    ("Age".to_string(), Box::new(Type::Int64))
                ],
                skip_exact:        vec!["temp.field".to_string()],
                skip_regex:        vec![".*\\.debug".to_string()],
            }
        );
    }

    /// Tests parsing of JSON with quoted skip paths
    #[test]
    fn test_json_quoted_skip_paths() {
        // Test single-quoted skip paths
        let json_type = Type::from_str("JSON(SKIP 'field.to.skip')").unwrap();
        if let Type::JSON { skip_exact, skip_regex, .. } = json_type {
            assert_eq!(skip_exact.len(), 1);
            assert_eq!(skip_exact[0], "field.to.skip");
            assert!(skip_regex.is_empty());
        } else {
            panic!("Expected JSON type");
        }

        // Test double-quoted skip paths
        let json_type = Type::from_str("JSON(SKIP \"another.field\")").unwrap();
        if let Type::JSON { skip_exact, skip_regex, .. } = json_type {
            assert_eq!(skip_exact.len(), 1);
            assert_eq!(skip_exact[0], "another.field");
            assert!(skip_regex.is_empty());
        } else {
            panic!("Expected JSON type");
        }

        // Test backtick-quoted skip paths (existing behavior)
        let json_type = Type::from_str("JSON(SKIP `backtick.field`)").unwrap();
        if let Type::JSON { skip_exact, skip_regex, .. } = json_type {
            assert_eq!(skip_exact.len(), 1);
            assert_eq!(skip_exact[0], "backtick.field");
            assert!(skip_regex.is_empty());
        } else {
            panic!("Expected JSON type");
        }

        // Test mixed quotes with multiple skip paths
        let json_type =
            Type::from_str("JSON(SKIP 'single', SKIP \"double\", SKIP `backtick`)").unwrap();
        if let Type::JSON { skip_exact, skip_regex, .. } = json_type {
            assert_eq!(skip_exact.len(), 3);
            assert!(skip_regex.is_empty());
            assert_eq!(skip_exact[0], "single");
            assert_eq!(skip_exact[1], "double");
            assert_eq!(skip_exact[2], "backtick");
        } else {
            panic!("Expected JSON type");
        }

        // Test combined with other parameters
        let json_type =
            Type::from_str("JSON(max_dynamic_paths=100, SKIP 'field.name', Name String)").unwrap();
        if let Type::JSON { max_dynamic_paths, skip_exact, skip_regex, typed_paths, .. } = json_type {
            assert_eq!(max_dynamic_paths, Some(100));
            assert_eq!(skip_exact.len(), 1);
            assert_eq!(skip_exact[0], "field.name");
            assert!(skip_regex.is_empty());
            assert_eq!(typed_paths.len(), 1);
            assert_eq!(typed_paths[0], ("Name".to_string(), Box::new(Type::String)));
        } else {
            panic!("Expected JSON type");
        }
    }

    /// Tests parsing of JSON typed paths and skip paths.
    #[test]
    fn test_json_typed_and_skip_paths() {
        // Test basic typed paths
        let json_type = Type::from_str("JSON(Name String, Age UInt32, Score Float64)").unwrap();
        if let Type::JSON { typed_paths, skip_exact, skip_regex, .. } = json_type {
            assert_eq!(typed_paths.len(), 3);
            assert_eq!(typed_paths[0], ("Name".to_string(), Box::new(Type::String)));
            assert_eq!(typed_paths[1], ("Age".to_string(), Box::new(Type::UInt32)));
            assert_eq!(typed_paths[2], ("Score".to_string(), Box::new(Type::Float64)));
            assert!(skip_exact.is_empty());
            assert!(skip_regex.is_empty());
        } else {
            panic!("Expected JSON type");
        }

        // Test basic skip paths
        let json_type = Type::from_str("JSON(SKIP debug.info, SKIP REGEXP '.*\\.temp')").unwrap();
        if let Type::JSON { typed_paths, skip_exact, skip_regex, .. } = json_type {
            assert!(typed_paths.is_empty());
            assert_eq!(skip_exact, vec!["debug.info".to_string()]);
            assert_eq!(skip_regex, vec![".*\\.temp".to_string()]);
        } else {
            panic!("Expected JSON type");
        }

        // Test complex nested types in typed paths
        let json_type =
            Type::from_str("JSON(UserData Nullable(String), Scores Array(Float64))").unwrap();
        if let Type::JSON { typed_paths, .. } = json_type {
            assert_eq!(typed_paths.len(), 2);
            assert_eq!(
                typed_paths[0],
                ("UserData".to_string(), Box::new(Type::Nullable(Box::new(Type::String))))
            );
            assert_eq!(
                typed_paths[1],
                ("Scores".to_string(), Box::new(Type::Array(Box::new(Type::Float64))))
            );
        } else {
            panic!("Expected JSON type");
        }

        // Test backtick-quoted field names
        let json_type = Type::from_str("JSON(`user.name` String, `data.count` Int32)").unwrap();
        if let Type::JSON { typed_paths, .. } = json_type {
            assert_eq!(typed_paths.len(), 2);
            assert_eq!(typed_paths[0], ("user.name".to_string(), Box::new(Type::String)));
            assert_eq!(typed_paths[1], ("data.count".to_string(), Box::new(Type::Int32)));
        } else {
            panic!("Expected JSON type");
        }

        // Test invalid type in typed path should be silently ignored
        let json_type =
            Type::from_str("JSON(ValidName String, InvalidType UnknownType, AnotherValid UInt64)")
                .unwrap();
        if let Type::JSON { typed_paths, .. } = json_type {
            // Should only have the valid types
            assert_eq!(typed_paths.len(), 2);
            assert_eq!(typed_paths[0], ("ValidName".to_string(), Box::new(Type::String)));
            assert_eq!(typed_paths[1], ("AnotherValid".to_string(), Box::new(Type::UInt64)));
        } else {
            panic!("Expected JSON type");
        }
    }

    /// Tests round-trip serialization/parsing of parameterized types.
    #[test]
    fn test_round_trip_parameterized_types() {
        // Test that Display and FromStr are consistent for parameterized types
        let test_cases = vec![
            Type::Dynamic { max_types: None },
            Type::Dynamic { max_types: Some(16) },
            Type::Dynamic { max_types: Some(100) },
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            },
            Type::JSON {
                max_dynamic_paths: Some(16),
                max_dynamic_types: None,
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            },
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: Some(64),
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            },
            Type::JSON {
                max_dynamic_paths: Some(100),
                max_dynamic_types: Some(32),
                typed_paths:       vec![],
                skip_exact:        vec![],
                skip_regex:        vec![],
            },
            Type::JSON {
                max_dynamic_paths: None,
                max_dynamic_types: None,
                typed_paths:       vec![("Name".to_string(), Box::new(Type::String))],
                skip_exact:        vec!["debug.field".to_string()],
                skip_regex:        vec![],
            },
        ];

        for original_type in test_cases {
            let type_string = original_type.to_string();
            let parsed_type = Type::from_str(&type_string)
                .unwrap_or_else(|e| panic!("Failed to parse '{type_string}': {e}"));
            assert_eq!(
                original_type, parsed_type,
                "Round-trip failed: {original_type} -> {type_string} -> {parsed_type}"
            );
        }
    }

    /// Tests parsing of simple Variant types
    #[test]
    fn test_parse_simple_variant() {
        let variant = Type::from_str("Variant(String, UInt64, Date)").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 3);
                assert_eq!(types[0], Type::String);
                assert_eq!(types[1], Type::UInt64);
                assert_eq!(types[2], Type::Date);
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of nested Variant types
    #[test]
    fn test_parse_nested_variant() {
        let variant = Type::from_str("Variant(String, Variant(UInt64, Date))").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0], Type::String);

                // Check the nested variant
                match &types[1] {
                    Type::Variant(inner_types) => {
                        assert_eq!(inner_types.len(), 2);
                        assert_eq!(inner_types[0], Type::UInt64);
                        assert_eq!(inner_types[1], Type::Date);
                    }
                    _ => panic!("Expected nested Variant type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of deeply nested Variant types
    #[test]
    fn test_parse_deeply_nested_variant() {
        let variant =
            Type::from_str("Variant(String, Variant(UInt64, Variant(Date, Float32)))").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0], Type::String);

                // Check the first level nested variant
                match &types[1] {
                    Type::Variant(inner_types) => {
                        assert_eq!(inner_types.len(), 2);
                        assert_eq!(inner_types[0], Type::UInt64);

                        // Check the second level nested variant
                        match &inner_types[1] {
                            Type::Variant(deep_types) => {
                                assert_eq!(deep_types.len(), 2);
                                assert_eq!(deep_types[0], Type::Date);
                                assert_eq!(deep_types[1], Type::Float32);
                            }
                            _ => panic!("Expected deeply nested Variant type"),
                        }
                    }
                    _ => panic!("Expected nested Variant type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of Variant with other complex types
    #[test]
    fn test_parse_variant_with_other_complex_types() {
        // Variant containing Array and Nullable types
        let variant = Type::from_str("Variant(String, Array(UInt64), Nullable(Date))").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 3);
                assert_eq!(types[0], Type::String);

                match &types[1] {
                    Type::Array(inner) => {
                        assert_eq!(**inner, Type::UInt64);
                    }
                    _ => panic!("Expected Array type"),
                }

                match &types[2] {
                    Type::Nullable(inner) => {
                        assert_eq!(**inner, Type::Date);
                    }
                    _ => panic!("Expected Nullable type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of Variant containing a Tuple
    #[test]
    fn test_parse_variant_with_tuple() {
        let variant = Type::from_str("Variant(String, Tuple(UInt64, Date))").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0], Type::String);

                match &types[1] {
                    Type::Tuple(inner_types) => {
                        assert_eq!(inner_types.len(), 2);
                        assert_eq!(inner_types[0], Type::UInt64);
                        assert_eq!(inner_types[1], Type::Date);
                    }
                    _ => panic!("Expected Tuple type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of Variant containing a Map
    #[test]
    fn test_parse_variant_with_map() {
        let variant = Type::from_str("Variant(String, Map(String, UInt64))").unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 2);
                assert_eq!(types[0], Type::String);

                match &types[1] {
                    Type::Map(key, value) => {
                        assert_eq!(**key, Type::String);
                        assert_eq!(**value, Type::UInt64);
                    }
                    _ => panic!("Expected Map type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests parsing of a complex nested Variant with multiple levels
    #[test]
    fn test_parse_complex_nested_variant() {
        let variant = Type::from_str(
            "Variant(String, Array(Variant(UInt64, Nullable(Date))), Map(String, Variant(Float32, \
             Bool)))",
        )
        .unwrap();
        match variant {
            Type::Variant(types) => {
                assert_eq!(types.len(), 3);
                assert_eq!(types[0], Type::String);

                // Check Array of Variant
                match &types[1] {
                    Type::Array(inner) => match &**inner {
                        Type::Variant(var_types) => {
                            assert_eq!(var_types.len(), 2);
                            assert_eq!(var_types[0], Type::UInt64);
                            match &var_types[1] {
                                Type::Nullable(nullable_inner) => {
                                    assert_eq!(**nullable_inner, Type::Date);
                                }
                                _ => panic!("Expected Nullable type"),
                            }
                        }
                        _ => panic!("Expected Variant type inside Array"),
                    },
                    _ => panic!("Expected Array type"),
                }

                // Check Map with Variant value
                match &types[2] {
                    Type::Map(key, value) => {
                        assert_eq!(**key, Type::String);
                        match &**value {
                            Type::Variant(var_types) => {
                                assert_eq!(var_types.len(), 2);
                                assert_eq!(var_types[0], Type::Float32);
                                assert_eq!(var_types[1], Type::UInt8); // Bool is UInt8
                            }
                            _ => panic!("Expected Variant type as Map value"),
                        }
                    }
                    _ => panic!("Expected Map type"),
                }
            }
            _ => panic!("Expected Variant type"),
        }
    }

    /// Tests that empty Variant should fail
    #[test]
    fn test_parse_variant_empty_should_fail() {
        let result = Type::from_str("Variant()");
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("at least one type argument"));
        }
    }
}
