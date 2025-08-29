- question:
  - when inserting into a JSON column that has predefined types for specific paths do we need to properly handle that on the client?
    if yes - do we already?
- For example,  JsonSerializer  still has its own  group_values_by_type ,  write_discriminators_internal_... , and  
     other helpers that are almost identical to the ones in  DynamicSerializer .                                         
- deal with deprecated JSON fields - refactor to fully reuse Dynamic functionality and get rid of duplicative code
- we should generate golden files from --format Variant queries to verify how we're serializing to match clickhouse
  or just run commands directly and diff?
