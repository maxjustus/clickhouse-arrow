/// Shared utilities for handling discriminators in Dynamic and JSON serialization
///
/// Discriminators are variable-sized indices used to identify types in columnar format.
/// The size depends on the total number of types:
/// - 0..=255 types: u8
/// - 256..=65535 types: u16
/// - 65536..=4294967295 types: u32
/// - More: u64

/// Macro to write discriminator based on total types count
/// Automatically selects the appropriate size (u8/u16/u32/u64)
#[macro_export]
macro_rules! write_discriminator {
    (async $writer:expr, $disc:expr, $total_types:expr) => {{
        let disc_val: u64 = $disc;
        let total: usize = $total_types;
        match total {
            0..=255 => {
                debug_assert!(disc_val <= 255);
                $writer.write_u8(u8::try_from(disc_val).unwrap()).await?
            }
            256..=65535 => {
                debug_assert!(disc_val <= 65535);
                $writer.write_u16_le(u16::try_from(disc_val).unwrap()).await?
            }
            65536..=4_294_967_295 => $writer.write_u32_le(u32::try_from(disc_val).unwrap()).await?,
            _ => $writer.write_u64_le(disc_val).await?,
        }
    }};
    (sync $writer:expr, $disc:expr, $total_types:expr) => {{
        let disc_val: u64 = $disc;
        let total: usize = $total_types;
        match total {
            0..=255 => {
                debug_assert!(disc_val <= 255);
                $writer.put_u8(u8::try_from(disc_val).unwrap())
            }
            256..=65535 => {
                debug_assert!(disc_val <= 65535);
                $writer.put_u16_le(u16::try_from(disc_val).unwrap())
            }
            65536..=4_294_967_295 => $writer.put_u32_le(u32::try_from(disc_val).unwrap()),
            _ => $writer.put_u64_le(disc_val),
        }
    }};
}
