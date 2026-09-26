//! ISO 3166-1 alpha-2 validation for the `location` setting.

/// Country codes accepted by the reference API (the `country-list` package it depends on).
const COUNTRY_CODES: &str = "AD AE AF AG AI AL AM AO AQ AR AS AT AU AW AX AZ BA BB BD BE BF BG BH BI BJ BL \
BM BN BO BQ BR BS BT BV BW BY BZ CA CC CD CF CG CH CI CK CL CM CN CO CR CU CV CW CX CY CZ DE DJ DK DM DO \
DZ EC EE EG EH ER ES ET FI FJ FK FM FO FR GA GB GD GE GF GG GH GI GL GM GN GP GQ GR GS GT GU GW GY HK HM \
HN HR HT HU ID IE IL IM IN IO IQ IR IS IT JE JM JO JP KE KG KH KI KM KN KP KR KW KY KZ LA LB LC LI LK LR \
LS LT LU LV LY MA MC MD ME MF MG MH MK ML MM MN MO MP MQ MR MS MT MU MV MW MX MY MZ NA NC NE NF NG NI NL \
NO NP NR NU NZ OM PA PE PF PG PH PK PL PM PN PR PS PT PW PY QA RE RO RS RU RW SA SB SC SD SE SG SH SI SJ \
SK SL SM SN SO SR SS ST SV SX SY SZ TC TD TF TG TH TJ TK TL TM TN TO TR TT TV TW TZ UA UG UM US UY UZ VA \
VC VE VG VI VN VU WF WS YE YT ZA ZM ZW";

/// An empty location is valid (not advertised); otherwise it must be a known country
/// code, compared case-insensitively.
pub fn is_valid_location_code(code: &str) -> bool {
    if code.is_empty() {
        return true;
    }
    let upper = code.to_ascii_uppercase();
    COUNTRY_CODES.split_ascii_whitespace().any(|known| known == upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_codes() {
        assert!(is_valid_location_code(""));
        assert!(is_valid_location_code("GB"));
        assert!(is_valid_location_code("it"));
        assert!(!is_valid_location_code("UK"));
        assert!(!is_valid_location_code("GBR"));
        assert_eq!(COUNTRY_CODES.split_ascii_whitespace().count(), 249);
    }
}
