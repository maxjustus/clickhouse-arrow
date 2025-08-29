We should mirror how ch-go verifies behavior against clickhouse local using Native format

## Example:

`
clickhouse local --format Native "select map('a', 'b' || toString(number))::JSON(a LowCardinality(String)) as z from system.numbers 
limit 5 settings output_format_native_use_flattened_dynamic_and_json_serialization=1, use_variant_as_common_type=1"
`
