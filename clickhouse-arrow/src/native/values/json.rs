use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Error, FromSql, Result, ToSql, Type, Value};

/// A `Vec` wrapper that is encoded as a tuple in SQL as opposed to a Vec
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Json<T>(pub T);

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(self, _type_hint: Option<&Type>) -> Result<Value> {
        #[cfg(feature = "serde")]
        {
            // Prefer structured JSON when available
            let v = serde_json::to_value(&self.0)
                .map_err(|e| Error::SerializeError(e.to_string()))?;
            return Ok(Value::Json(v));
        }
        #[cfg(not(feature = "serde"))]
        {
            Ok(Value::Object(
                serde_json::to_string(&self.0)
                    .map_err(|e| Error::SerializeError(e.to_string()))?
                    .into_bytes(),
            ))
        }
    }
}

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn from_sql(type_: &Type, value: Value) -> Result<Self> {
        #[cfg(feature = "serde")]
        {
            match value {
                Value::Json(v) => {
                    return Ok(Json(serde_json::from_value(v)
                        .map_err(|e| Error::DeserializeError(e.to_string()))?));
                }
                Value::Object(x) | Value::String(x) => {
                    return Ok(Json(serde_json::from_slice(&x)
                        .map_err(|e| Error::DeserializeError(e.to_string()))?));
                }
                other => {
                    // Fallback via string path for unexpected variants
                    let raw: String = FromSql::from_sql(type_, other)?;
                    return Ok(Json(serde_json::from_str(&raw)
                        .map_err(|e| Error::DeserializeError(e.to_string()))?));
                }
            }
        }
        #[cfg(not(feature = "serde"))]
        {
            let raw: String = FromSql::from_sql(type_, value)?;
            Ok(Json(serde_json::from_str(&raw)
                .map_err(|e| Error::DeserializeError(e.to_string()))?))
        }
    }
}
