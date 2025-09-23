use ::serde::de::DeserializeOwned;
use ::serde::{Deserialize, Serialize};

use crate::{Error, FromSql, Result, ToSql, Type, Value};

/// A `Vec` wrapper that is encoded as a tuple in SQL as opposed to a Vec
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Json<T>(pub T);

impl<T: Serialize> ToSql for Json<T> {
    fn to_sql(self, _type_hint: Option<&Type>) -> Result<Value> {
        let v = serde_json::to_value(&self.0).map_err(|e| Error::SerializeError(e.to_string()))?;
        Ok(Value::Json(v))
    }
}

impl<T: DeserializeOwned> FromSql for Json<T> {
    fn from_sql(type_: &Type, value: Value) -> Result<Self> {
        match value {
            Value::Json(v) => Ok(Json(
                serde_json::from_value(v).map_err(|e| Error::DeserializeError(e.to_string()))?,
            )),
            Value::Object(x) | Value::String(x) => Ok(Json(
                serde_json::from_slice(&x).map_err(|e| Error::DeserializeError(e.to_string()))?,
            )),
            other => {
                let raw: String = FromSql::from_sql(type_, other)?;
                Ok(Json(
                    serde_json::from_str(&raw)
                        .map_err(|e| Error::DeserializeError(e.to_string()))?,
                ))
            }
        }
    }
}
