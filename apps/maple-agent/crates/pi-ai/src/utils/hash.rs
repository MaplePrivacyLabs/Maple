//! Port of `packages/ai/src/utils/hash.ts`.

/// Pi's fast deterministic hash, operating on JavaScript UTF-16 code units.
pub fn short_hash(value: impl Into<super::js_value::JsString>) -> String {
    let value = value.into();
    let mut h1 = 0xdead_beef_u32;
    let mut h2 = 0x41c6_ce57_u32;
    for character in value.units() {
        h1 = (h1 ^ u32::from(character)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(character)).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut number: u32) -> String {
    if number == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while number > 0 {
        digits.push(char::from_digit(number % 36, 36).expect("base-36 digit"));
        number /= 36;
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_pinned_pi_utf16_vectors() {
        // Evaluated from Pi v1.0.4's shortHash using the pinned Node 22.23.2.
        for (input, expected) in [
            ("", "k4n83c7h0j2b"),
            ("hello", "1h6qa0qrowduu"),
            ("🙈", "kphsz0153ms3q"),
            ("A🙈é", "956afvqgp71m"),
            ("𝄞", "18f4y91gwgbpv"),
            ("\0\u{ffff}", "r727dd26v726"),
        ] {
            assert_eq!(short_hash(input), expected);
        }
    }

    #[test]
    fn hash_preserves_lone_surrogate_code_units() {
        // Evaluated from the pinned Pi source, including raw invalid UTF-16.
        for (units, expected) in [
            (vec![0xd800], "sjkthq29gslh"),
            (vec![0xdc00], "1czx0omv4xxbk"),
            (vec![0xd800, 0x61, 0xdc00], "bmuyxr1etstb"),
        ] {
            assert_eq!(
                short_hash(super::super::js_value::JsString::from_utf16(units)),
                expected
            );
        }
    }
}
