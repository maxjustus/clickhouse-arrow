use std::collections::HashMap;

use crate::Result;
use crate::native::types::Type;

/// Parse a type entry from type name bytes (used by Dynamic and JSON deserializers)
pub(crate) fn parse_type_entry(type_name_bytes: Vec<u8>) -> Result<(String, Type)> {
    let type_name = String::from_utf8(type_name_bytes).map_err(|e| {
        crate::Error::DeserializeError(format!("Invalid UTF-8 in type name: {e}"))
    })?;
    let typ = type_name
        .parse::<Type>()
        .map_err(|_| crate::Error::DeserializeError(format!("Unknown type: {type_name}")))?;
    Ok((type_name, typ))
}

/// Build offset mapping and count rows per type (used by Dynamic and JSON deserializers)
pub(crate) fn build_offsets(
    discriminators: &[u64],
    total_types: u64,
) -> (Vec<usize>, HashMap<u64, usize>) {
    let mut row_count_by_type = HashMap::new();
    let mut offsets = vec![0; discriminators.len()];

    for (i, &disc) in discriminators.iter().enumerate() {
        if disc != total_types {
            // NULL discriminator is total_types
            let count = row_count_by_type.entry(disc).or_default();
            offsets[i] = *count;
            *count += 1;
        }
    }

    (offsets, row_count_by_type)
}