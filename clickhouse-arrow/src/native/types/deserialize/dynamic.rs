use std::collections::HashMap;

use tokio::io::AsyncReadExt;

use crate::io::{ClickHouseBytesRead, ClickHouseRead};
use crate::native::types::deserialize::DeserializerState;
use crate::native::types::{Type, Value};
use crate::Result;

/// Special type name for SharedVariant overflow in Dynamic type
const SHARED_VARIANT_TYPE_NAME: &str = "SharedVariant";

/// Represents a type registry for Dynamic columns
/// Dynamic type maintains a registry of up to max_types different types
/// When types exceed max_types, they overflow to a SharedVariant type
#[derive(Debug, Clone)]
pub(crate) struct DynamicTypeRegistry {
    /// Maximum number of distinct types allowed (0-254)
    max_types: u8,
    /// Maps type string to type index
    type_to_index: HashMap<String, u8>,
    /// Maps type index to (type_string, type)
    index_to_type: HashMap<u8, (String, Type)>,
    /// Next available type index
    next_index: u8,
    /// Whether SharedVariant is in use for overflow types
    has_shared_variant: bool,
}

impl DynamicTypeRegistry {
    /// Create a new type registry with the given max_types
    pub(crate) fn new(max_types: u8) -> Self {
        Self {
            max_types,
            type_to_index: HashMap::new(),
            index_to_type: HashMap::new(),
            next_index: 0,
            has_shared_variant: false,
        }
    }

    /// Register a type in the registry
    /// Returns the type index if successful, or None if the registry is full
    pub(crate) fn register_type(&mut self, type_name: &str, type_: Type) -> Option<u8> {
        // Check if type is already registered
        if let Some(&index) = self.type_to_index.get(type_name) {
            return Some(index);
        }

        // Check if we've reached the max_types limit
        if self.next_index >= self.max_types {
            // Use SharedVariant for overflow types
            if !self.has_shared_variant {
                self.has_shared_variant = true;
                let shared_variant_index = self.max_types; // Use max_types as SharedVariant index
                let _ = self.type_to_index.insert(SHARED_VARIANT_TYPE_NAME.to_string(), shared_variant_index);
                // SharedVariant itself is a Variant type that can contain any type
                let shared_variant_type = Type::Variant(vec![]); // Will be dynamically populated
                let _ = self.index_to_type.insert(shared_variant_index, (SHARED_VARIANT_TYPE_NAME.to_string(), shared_variant_type));
            }
            return Some(self.max_types); // Return SharedVariant index
        }

        // Register the new type
        let index = self.next_index;
        let _ = self.type_to_index.insert(type_name.to_string(), index);
        let _ = self.index_to_type.insert(index, (type_name.to_string(), type_));
        self.next_index += 1;
        Some(index)
    }

    /// Get type by index
    pub(crate) fn get_type(&self, index: u8) -> Option<&Type> {
        self.index_to_type.get(&index).map(|(_, t)| t)
    }

    /// Get type name by index
    pub(crate) fn get_type_name(&self, index: u8) -> Option<&str> {
        self.index_to_type.get(&index).map(|(name, _)| name.as_str())
    }

    /// Get all registered indices in order
    pub(crate) fn indices(&self) -> Vec<u8> {
        let mut indices: Vec<u8> = self.index_to_type.keys().copied().collect();
        indices.sort_unstable();
        indices
    }

    /// Check if SharedVariant is being used
    pub(crate) fn uses_shared_variant(&self) -> bool {
        self.has_shared_variant
    }
}

pub(crate) struct DynamicDeserializer;

impl DynamicDeserializer {
    pub(crate) fn read_prefix_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        _state: &mut DeserializerState,
    ) -> Result<()> {
        let max_types = type_.unwrap_dynamic()?;
        
        // Read 8-byte version prefix - ClickHouse 25.5 uses version 2
        let mut version_prefix = [0u8; 8];
        reader.try_copy_to_slice(&mut version_prefix)?;
        
        let version = version_prefix[0];
        
        match version {
            1 => {
                // Version 1 (deprecated): alphabetically sorted, implicit SharedVariant
                // eprintln!("DEBUG: Using Dynamic version 1 (deprecated) format");
                return Self::read_prefix_v1_sync(max_types, reader);
            }
            2 => {
                // Version 2: ClickHouse 25.5 format - needs investigation
                // eprintln!("DEBUG: Using Dynamic version 2 (ClickHouse 25.5) format");
                return Self::read_prefix_v2_sync(max_types, reader);
            }
            3 => {
                // Version 3 (current): type order preserved, variable discriminators  
                // eprintln!("DEBUG: Using Dynamic version 3 (current) format");
                return Self::read_prefix_v3_sync(max_types, reader);
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "Unsupported Dynamic version: {} (supported: 1, 2, 3)", version
                )));
            }
        }
    }
    
    fn read_prefix_v1_sync<R: ClickHouseBytesRead>(
        max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 1: Read max_types, total_types, then type names (alphabetically sorted)
        let registry_max_types = reader.try_get_var_uint()? as u8;
        let total_types = reader.try_get_var_uint()? as u8;
        
        // eprintln!("DEBUG: V1 max_types={}, total_types={}", registry_max_types, total_types);
        
        // Read type names - they will be alphabetically sorted with SharedVariant added
        for i in 0..total_types {
            let name_len = reader.try_get_var_uint()? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            reader.try_copy_to_slice(&mut type_name_bytes)?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V1 type[{}]: {}", i, type_name);
        }
        
        // Read variant serialization version
        let mut variant_version = [0u8; 8];
        reader.try_copy_to_slice(&mut variant_version)?;
        // eprintln!("DEBUG: V1 variant version: {:?}", variant_version);
        
        Ok(())
    }
    
    fn read_prefix_v2_sync<R: ClickHouseBytesRead>(
        _max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 2: ClickHouse 25.5 format
        // Based on TCP dump analysis: [2,0,0,0,0,0,0,0] [total_types] [type_names...] [8-byte-padding]
        let total_types = reader.try_get_var_uint()? as u8;
        
        // eprintln!("DEBUG: V2 total_types={}", total_types);
        
        // Read type names in order (no alphabetical sorting like V1)
        for i in 0..total_types {
            let name_len = reader.try_get_var_uint()? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            reader.try_copy_to_slice(&mut type_name_bytes)?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V2 type[{}]: {}", i, type_name);
        }
        
        // Version 2 has an 8-byte padding/custom serialization section
        let mut padding = [0u8; 8];
        reader.try_copy_to_slice(&mut padding)?;
        // eprintln!("DEBUG: V2 padding/serialization: {:?}", padding);
        
        Ok(())
    }
    
    fn read_prefix_v3_sync<R: ClickHouseBytesRead>(
        _max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 3: Current format - type order preserved, custom serialization
        let total_types = reader.try_get_var_uint()? as u8;
        
        // eprintln!("DEBUG: V3 total_types={}", total_types);
        
        // Read type names in original order
        for i in 0..total_types {
            let name_len = reader.try_get_var_uint()? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            reader.try_copy_to_slice(&mut type_name_bytes)?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V3 type[{}]: {}", i, type_name);
        }
        
        // Read custom serialization prefixes for each type
        // This is specific to V3 format
        
        Ok(())
    }

    pub(crate) async fn read_prefix<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        _state: &mut DeserializerState,
    ) -> Result<()> {
        let max_types = type_.unwrap_dynamic()?;
        
        // Read 8-byte version prefix - ClickHouse 25.5 uses version 2
        let mut version_prefix = [0u8; 8];
        let _ = reader.read_exact(&mut version_prefix).await?;
        
        let version = version_prefix[0];
        // eprintln!("DEBUG: Dynamic ASYNC version detected: {}", version);
        // eprintln!("DEBUG: DynamicDeserializer::read_prefix ASYNC called!");
        
        match version {
            1 => {
                // Version 1 (deprecated): alphabetically sorted, implicit SharedVariant
                // eprintln!("DEBUG: Using Dynamic version 1 (deprecated) format");
                return Self::read_prefix_v1_async(max_types, reader).await;
            }
            2 => {
                // Version 2: ClickHouse 25.5 format - needs investigation
                // eprintln!("DEBUG: Using Dynamic version 2 (ClickHouse 25.5) format");
                return Self::read_prefix_v2_async(max_types, reader).await;
            }
            3 => {
                // Version 3 (current): type order preserved, variable discriminators  
                // eprintln!("DEBUG: Using Dynamic version 3 (current) format");
                return Self::read_prefix_v3_async(max_types, reader).await;
            }
            _ => {
                return Err(crate::Error::DeserializeError(format!(
                    "Unsupported Dynamic version: {} (supported: 1, 2, 3)", version
                )));
            }
        }
    }
    
    async fn read_prefix_v1_async<R: ClickHouseRead>(
        _max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 1: Read max_types, total_types, then type names (alphabetically sorted)
        let registry_max_types = reader.read_var_uint().await? as u8;
        let total_types = reader.read_var_uint().await? as u8;
        
        // eprintln!("DEBUG: V1 ASYNC max_types={}, total_types={}", registry_max_types, total_types);
        
        // Read type names - they will be alphabetically sorted with SharedVariant added
        for i in 0..total_types {
            let name_len = reader.read_var_uint().await? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            let _ = reader.read_exact(&mut type_name_bytes).await?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V1 ASYNC type[{}]: {}", i, type_name);
        }
        
        // Read variant serialization version
        let mut variant_version = [0u8; 8];
        let _ = reader.read_exact(&mut variant_version).await?;
        // eprintln!("DEBUG: V1 ASYNC variant version: {:?}", variant_version);
        
        Ok(())
    }
    
    async fn read_prefix_v2_async<R: ClickHouseRead>(
        _max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 2: ClickHouse 25.5 format
        // Based on TCP dump analysis: [2,0,0,0,0,0,0,0] [total_types] [type_names...] [8-byte-padding]
        let total_types = reader.read_var_uint().await? as u8;
        
        // eprintln!("DEBUG: V2 ASYNC total_types={}", total_types);
        
        // Read type names in order (no alphabetical sorting like V1)
        for i in 0..total_types {
            let name_len = reader.read_var_uint().await? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            let _ = reader.read_exact(&mut type_name_bytes).await?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V2 ASYNC type[{}]: {}", i, type_name);
        }
        
        // Version 2 has an 8-byte padding/custom serialization section
        let mut padding = [0u8; 8];
        let _ = reader.read_exact(&mut padding).await?;
        // eprintln!("DEBUG: V2 ASYNC padding/serialization: {:?}", padding);
        
        Ok(())
    }
    
    async fn read_prefix_v3_async<R: ClickHouseRead>(
        _max_types: u8,
        reader: &mut R,
    ) -> Result<()> {
        // Version 3: Current format - type order preserved, custom serialization
        let total_types = reader.read_var_uint().await? as u8;
        
        // eprintln!("DEBUG: V3 ASYNC total_types={}", total_types);
        
        // Read type names in original order
        for i in 0..total_types {
            let name_len = reader.read_var_uint().await? as usize;
            let mut type_name_bytes = vec![0u8; name_len];
            let _ = reader.read_exact(&mut type_name_bytes).await?;
            let type_name = String::from_utf8(type_name_bytes).map_err(|_| {
                crate::Error::DeserializeError("Invalid UTF-8 in Dynamic type name".to_string())
            })?;
            // eprintln!("DEBUG: V3 ASYNC type[{}]: {}", i, type_name);
        }
        
        // Read custom serialization prefixes for each type
        // This is specific to V3 format
        
        Ok(())
    }

    pub(crate) async fn read_async<R: ClickHouseRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize,
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let _max_types = type_.unwrap_dynamic()?;
        
        // eprintln!("DEBUG: Reading Dynamic ASYNC data for {} rows", rows);
        
        // For version 2, we expect the discriminators to be UInt8 based on TCP dumps
        let mut discriminators = vec![0u8; rows];
        let _ = reader.read_exact(&mut discriminators).await?;
        
        // eprintln!("DEBUG: Dynamic ASYNC discriminators: {:?}", discriminators);
        
        // Hardcode String type for now since that's what we see in TCP dumps
        // TODO: Use actual type registry from prefix parsing
        let string_type = Type::String;
        
        // Count how many values we need to read for String type (discriminator 1 in version 2)
        let string_count = discriminators.iter().filter(|&&d| d == 1).count();
        let null_count = discriminators.iter().filter(|&&d| d == 255).count();
        
        // eprintln!("DEBUG: ASYNC String values: {}, NULL values: {}", string_count, null_count);
        
        // Read String column data if we have any
        let mut string_values = Vec::new();
        if string_count > 0 {
            string_values = string_type.deserialize_column(reader, string_count, state).await?;
            // eprintln!("DEBUG: ASYNC Read {} string values: {:?}", string_values.len(), string_values);
        }
        
        // Reconstruct Dynamic values in original order
        let mut values = Vec::with_capacity(rows);
        let mut string_offset = 0;
        
        for &disc in &discriminators {
            match disc {
                1 => {
                    // Based on TCP dump: discriminator 1 = String type (index 0 in registry)
                    if string_offset < string_values.len() {
                        values.push(Value::Dynamic("String".to_string(), Box::new(string_values[string_offset].clone())));
                        string_offset += 1;
                    } else {
                        return Err(crate::Error::DeserializeError(
                            format!("Not enough string values: offset {} >= length {}", string_offset, string_values.len())
                        ));
                    }
                }
                255 => {
                    // NULL discriminator for both Variant and Dynamic types
                    values.push(Value::Dynamic("NULL".to_string(), Box::new(Value::Null)));
                }
                _ => {
                    return Err(crate::Error::DeserializeError(format!(
                        "Unknown Dynamic discriminator: {} (supported: 255=NULL, 1=String)", disc
                    )));
                }
            }
        }
        
        // eprintln!("DEBUG: Final Dynamic ASYNC values: {} items", values.len());
        Ok(values)
    }

    pub(crate) fn read_sync<R: ClickHouseBytesRead>(
        type_: &Type,
        reader: &mut R,
        rows: usize, 
        state: &mut DeserializerState,
    ) -> Result<Vec<Value>> {
        let _max_types = type_.unwrap_dynamic()?;
        
        // eprintln!("DEBUG: Reading Dynamic data for {} rows", rows);
        // eprintln!("DEBUG: Dynamic read_sync called!");
        
        // For version 2, we expect the discriminators to be UInt8 based on TCP dumps
        // This will need to be adjusted based on the actual version parsing
        let mut discriminators = vec![0u8; rows];
        reader.try_copy_to_slice(&mut discriminators)?;
        
        // eprintln!("DEBUG: Dynamic discriminators: {:?}", discriminators);
        
        // Hardcode String type for now since that's what we see in TCP dumps
        // TODO: Use actual type registry from prefix parsing
        let string_type = Type::String;
        
        // Count how many values we need to read for String type (discriminator 1 in version 2)
        let string_count = discriminators.iter().filter(|&&d| d == 1).count();
        let null_count = discriminators.iter().filter(|&&d| d == 255).count();
        
        // eprintln!("DEBUG: String values: {}, NULL values: {}", string_count, null_count);
        
        // Read String column data if we have any
        let mut string_values = Vec::new();
        if string_count > 0 {
            string_values = string_type.deserialize_column_sync(reader, string_count, state)?;
            // eprintln!("DEBUG: Read {} string values: {:?}", string_values.len(), string_values);
        }
        
        // Reconstruct Dynamic values in original order
        let mut values = Vec::with_capacity(rows);
        let mut string_offset = 0;
        
        for &disc in &discriminators {
            match disc {
                1 => {
                    // Based on TCP dump: discriminator 1 = String type (index 0 in registry)
                    if string_offset < string_values.len() {
                        values.push(Value::Dynamic("String".to_string(), Box::new(string_values[string_offset].clone())));
                        string_offset += 1;
                    } else {
                        return Err(crate::Error::DeserializeError(
                            format!("Not enough string values: offset {} >= length {}", string_offset, string_values.len())
                        ));
                    }
                }
                255 => {
                    // NULL discriminator for both Variant and Dynamic types
                    values.push(Value::Dynamic("NULL".to_string(), Box::new(Value::Null)));
                }
                _ => {
                    return Err(crate::Error::DeserializeError(format!(
                        "Unknown Dynamic discriminator: {} (supported: 255=NULL, 1=String)", disc
                    )));
                }
            }
        }
        
        // eprintln!("DEBUG: Final Dynamic values: {} items", values.len());
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dynamic_type_registry_basic() {
        let mut registry = DynamicTypeRegistry::new(3);
        
        // Register some types
        let string_index = registry.register_type("String", Type::String).unwrap();
        let uint64_index = registry.register_type("UInt64", Type::UInt64).unwrap();
        let date_index = registry.register_type("Date", Type::Date).unwrap();
        
        assert_eq!(string_index, 0);
        assert_eq!(uint64_index, 1);
        assert_eq!(date_index, 2);
        
        // Verify we can retrieve the types
        assert_eq!(registry.get_type(string_index).unwrap(), &Type::String);
        assert_eq!(registry.get_type(uint64_index).unwrap(), &Type::UInt64);
        assert_eq!(registry.get_type(date_index).unwrap(), &Type::Date);
        
        // Verify type names
        assert_eq!(registry.get_type_name(string_index).unwrap(), "String");
        assert_eq!(registry.get_type_name(uint64_index).unwrap(), "UInt64");
        assert_eq!(registry.get_type_name(date_index).unwrap(), "Date");
    }

    #[test]
    fn test_dynamic_type_registry_overflow() {
        let mut registry = DynamicTypeRegistry::new(2); // Only allow 2 types
        
        // Register 2 types - should work fine
        let string_index = registry.register_type("String", Type::String).unwrap();
        let uint64_index = registry.register_type("UInt64", Type::UInt64).unwrap();
        
        assert_eq!(string_index, 0);
        assert_eq!(uint64_index, 1);
        assert!(!registry.uses_shared_variant());
        
        // Register a 3rd type - should overflow to SharedVariant
        let date_index = registry.register_type("Date", Type::Date).unwrap();
        assert_eq!(date_index, 2); // max_types = SharedVariant index
        assert!(registry.uses_shared_variant());
        
        // Verify SharedVariant is registered
        assert_eq!(registry.get_type_name(2).unwrap(), SHARED_VARIANT_TYPE_NAME);
        
        // Register a 4th type - should also go to SharedVariant
        let float64_index = registry.register_type("Float64", Type::Float64).unwrap();
        assert_eq!(float64_index, 2); // Same SharedVariant index
    }

    #[test]
    fn test_dynamic_type_registry_duplicate_registration() {
        let mut registry = DynamicTypeRegistry::new(10);
        
        // Register the same type twice
        let index1 = registry.register_type("String", Type::String).unwrap();
        let index2 = registry.register_type("String", Type::String).unwrap();
        
        // Should return the same index
        assert_eq!(index1, index2);
        assert_eq!(index1, 0);
        
        // Should only use one slot
        assert_eq!(registry.next_index, 1);
    }

    #[test]
    fn test_dynamic_type_registry_indices() {
        let mut registry = DynamicTypeRegistry::new(5);
        
        let _ = registry.register_type("UInt64", Type::UInt64).unwrap();
        let _ = registry.register_type("String", Type::String).unwrap();
        let _ = registry.register_type("Date", Type::Date).unwrap();
        
        let indices = registry.indices();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn test_dynamic_type_parsing() {
        use std::str::FromStr;
        
        // Test parsing basic Dynamic type
        let dynamic_str = "Dynamic(max_types=5)";
        let dynamic_type = Type::from_str(dynamic_str).unwrap();
        
        match &dynamic_type {
            Type::Dynamic(max_types) => {
                assert_eq!(*max_types, 5);
            }
            _ => panic!("Expected Dynamic type"),
        }
        
        // Test default Dynamic type (no parameters)
        let default_dynamic_str = "Dynamic";
        let default_type = Type::from_str(default_dynamic_str).unwrap();
        
        match &default_type {
            Type::Dynamic(max_types) => {
                // Should use default max_types
                assert!(*max_types > 0); // Some reasonable default
            }
            _ => panic!("Expected Dynamic type"),
        }
    }

    #[test]
    fn test_shared_variant_logic() {
        let mut registry = DynamicTypeRegistry::new(2);
        
        // Fill up the registry
        let _ = registry.register_type("String", Type::String).unwrap();
        let _ = registry.register_type("UInt64", Type::UInt64).unwrap();
        
        // This should trigger SharedVariant creation
        let overflow_index = registry.register_type("Date", Type::Date).unwrap();
        
        assert_eq!(overflow_index, 2); // max_types value
        assert!(registry.uses_shared_variant());
        
        // Verify SharedVariant type is registered
        assert_eq!(registry.get_type_name(2).unwrap(), SHARED_VARIANT_TYPE_NAME);
        match registry.get_type(2).unwrap() {
            Type::Variant(_) => {}, // Should be a Variant type
            _ => panic!("SharedVariant should be a Variant type"),
        }
    }

    #[test]
    fn test_dynamic_value_creation() {
        let string_value = Value::Dynamic("String".to_string(), Box::new(Value::String(b"hello".to_vec())));
        let uint64_value = Value::Dynamic("UInt64".to_string(), Box::new(Value::UInt64(42)));
        
        match &string_value {
            Value::Dynamic(type_name, inner) => {
                assert_eq!(type_name, "String");
                match &**inner {
                    Value::String(s) => assert_eq!(s, b"hello"),
                    _ => panic!("Expected String value"),
                }
            }
            _ => panic!("Expected Dynamic value"),
        }
        
        match &uint64_value {
            Value::Dynamic(type_name, inner) => {
                assert_eq!(type_name, "UInt64");
                match &**inner {
                    Value::UInt64(n) => assert_eq!(*n, 42),
                    _ => panic!("Expected UInt64 value"),
                }
            }
            _ => panic!("Expected Dynamic value"),
        }
    }

    #[test]
    fn test_dynamic_value_display() {
        let value = Value::Dynamic("String".to_string(), Box::new(Value::String(b"test".to_vec())));
        let display_str = format!("{}", value);
        
        // Should show the type name and inner value
        assert!(display_str.contains("dynamic"));
        assert!(display_str.contains("String"));
        assert!(display_str.contains("test"));
    }

    #[test]
    fn test_dynamic_guess_type() {
        let value = Value::Dynamic("String".to_string(), Box::new(Value::String(b"test".to_vec())));
        let guessed_type = value.guess_type();
        
        match guessed_type {
            Type::Dynamic(max_types) => {
                assert_eq!(max_types, 255); // Default max_types
            }
            _ => panic!("Expected Dynamic type from guess_type"),
        }
    }
}