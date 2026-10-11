//! `js_number` against Node's `Number(text)`: the table is node v26 output
//! (regenerate with the inputs below and `Number(s)` printed as f64 bits).

use super::{js_number, js_number_to_string};

/// Node's result for one input.
#[derive(Debug, Clone, Copy, PartialEq)]
enum G {
    Nan,
    PosInf,
    NegInf,
    NegZero,
    /// Every other value, as its exact f64 bits.
    Bits(u64),
}

fn golden(value: f64) -> G {
    if value.is_nan() {
        G::Nan
    } else if value == f64::INFINITY {
        G::PosInf
    } else if value == f64::NEG_INFINITY {
        G::NegInf
    } else if value == 0.0 && value.is_sign_negative() {
        G::NegZero
    } else {
        G::Bits(value.to_bits())
    }
}

const NODE_GOLDENS: &[(&str, G)] = &[
    ("", G::Bits(0x0000_0000_0000_0000)),
    (" ", G::Bits(0x0000_0000_0000_0000)),
    (
        "\t\n\u{b}\u{c}\r \u{a0}\u{1680}\u{2000}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}",
        G::Bits(0x0000_0000_0000_0000),
    ),
    ("\u{85}", G::Nan),
    ("\u{85}5", G::Nan),
    ("5\u{85}", G::Nan),
    ("\u{180e}5", G::Nan),
    ("0", G::Bits(0x0000_0000_0000_0000)),
    ("-0", G::NegZero),
    ("+0", G::Bits(0x0000_0000_0000_0000)),
    (" 5 ", G::Bits(0x4014_0000_0000_0000)),
    ("\n42\n", G::Bits(0x4045_0000_0000_0000)),
    ("\u{feff}7", G::Bits(0x401c_0000_0000_0000)),
    ("1e3", G::Bits(0x408f_4000_0000_0000)),
    ("1E3", G::Bits(0x408f_4000_0000_0000)),
    ("1e+3", G::Bits(0x408f_4000_0000_0000)),
    ("1e-3", G::Bits(0x3f50_624d_d2f1_a9fc)),
    ("-1.5e-3", G::Bits(0xbf58_9374_bc6a_7efa)),
    ("+.5", G::Bits(0x3fe0_0000_0000_0000)),
    (".5", G::Bits(0x3fe0_0000_0000_0000)),
    ("5.", G::Bits(0x4014_0000_0000_0000)),
    ("-.5e1", G::Bits(0xc014_0000_0000_0000)),
    (".", G::Nan),
    ("+", G::Nan),
    ("-", G::Nan),
    ("e5", G::Nan),
    ("1e", G::Nan),
    ("1e+", G::Nan),
    ("1.2.3", G::Nan),
    ("1_000", G::Nan),
    ("1,000", G::Nan),
    ("Infinity", G::PosInf),
    ("+Infinity", G::PosInf),
    ("-Infinity", G::NegInf),
    (" Infinity ", G::PosInf),
    ("infinity", G::Nan),
    ("INFINITY", G::Nan),
    ("inf", G::Nan),
    ("-inf", G::Nan),
    ("+inf", G::Nan),
    ("nan", G::Nan),
    ("NaN", G::Nan),
    ("-NaN", G::Nan),
    ("Infinityx", G::Nan),
    ("0x10", G::Bits(0x4030_0000_0000_0000)),
    ("0X10", G::Bits(0x4030_0000_0000_0000)),
    ("0xff", G::Bits(0x406f_e000_0000_0000)),
    ("0xFF", G::Bits(0x406f_e000_0000_0000)),
    ("-0x10", G::Nan),
    ("+0x10", G::Nan),
    ("0x", G::Nan),
    ("0xg", G::Nan),
    ("0b101", G::Bits(0x4014_0000_0000_0000)),
    ("0B11", G::Bits(0x4008_0000_0000_0000)),
    ("0b2", G::Nan),
    ("0o17", G::Bits(0x402e_0000_0000_0000)),
    ("0O17", G::Bits(0x402e_0000_0000_0000)),
    ("0o8", G::Nan),
    ("017", G::Bits(0x4031_0000_0000_0000)),
    ("00", G::Bits(0x0000_0000_0000_0000)),
    ("0x1fffffffffffff", G::Bits(0x433f_ffff_ffff_ffff)),
    ("0x20000000000001", G::Bits(0x4340_0000_0000_0000)),
    ("0x20000000000003", G::Bits(0x4340_0000_0000_0002)),
    ("0x8000000000000001", G::Bits(0x43e0_0000_0000_0000)),
    (
        "0xffffffffffffffffffffffffffffffff",
        G::Bits(0x47f0_0000_0000_0000),
    ),
    (
        "0b1111111111111111111111111111111111111111111111111111111111111111111111",
        G::Bits(0x4450_0000_0000_0000),
    ),
    ("1e999", G::PosInf),
    ("-1e999", G::NegInf),
    ("1e-999", G::Bits(0x0000_0000_0000_0000)),
    ("4.9e-324", G::Bits(0x0000_0000_0000_0001)),
    ("2.4703282292062328e-324", G::Bits(0x0000_0000_0000_0001)),
    ("2.4703282292062327e-324", G::Bits(0x0000_0000_0000_0000)),
    ("0.1", G::Bits(0x3fb9_9999_9999_999a)),
    ("0.30000000000000004", G::Bits(0x3fd3_3333_3333_3334)),
    ("9007199254740993", G::Bits(0x4340_0000_0000_0000)),
    (
        "123456789012345678901234567890",
        G::Bits(0x45f8_ee90_ff6c_373e),
    ),
    ("1.7976931348623157e308", G::Bits(0x7fef_ffff_ffff_ffff)),
    ("1.7976931348623158e308", G::Bits(0x7fef_ffff_ffff_ffff)),
    ("1.7976931348623159e308", G::PosInf),
    ("15000", G::Bits(0x40cd_4c00_0000_0000)),
    ("15000.5", G::Bits(0x40cd_4c40_0000_0000)),
    ("-1", G::Bits(0xbff0_0000_0000_0000)),
    ("garbage", G::Nan),
    ("12abc", G::Nan),
    ("abc12", G::Nan),
    ("0.0000001", G::Bits(0x3e7a_d7f2_9abc_af48)),
    ("00012", G::Bits(0x4028_0000_0000_0000)),
    ("1e0003", G::Bits(0x408f_4000_0000_0000)),
    ("\u{663}", G::Nan),
    ("\u{ff15}", G::Nan),
    ("1 2", G::Nan),
    ("1\u{a0}", G::Bits(0x3ff0_0000_0000_0000)),
];

#[test]
fn matches_node_number_for_every_golden() {
    let actual: Vec<(&str, G)> = NODE_GOLDENS
        .iter()
        .map(|(input, _)| (*input, golden(js_number(input))))
        .collect();
    assert_eq!(actual, NODE_GOLDENS);
}

/// Node: `Number("0x" + "f".repeat(300))` is `Infinity`.
#[test]
fn an_oversized_hex_integer_overflows_to_infinity() {
    assert_eq!(
        golden(js_number(&format!("0x{}", "f".repeat(300)))),
        G::PosInf
    );
}

/// Node's `String(x)` for doubles given as exact bits (node v26:
/// `Buffer.writeDoubleBE(x)` printed beside `String(x)`).
const TO_STRING: &[(u64, &str)] = &[
    (0x0000_0000_0000_0000, "0"),
    (0x8000_0000_0000_0000, "0"),
    (0x3ff0_0000_0000_0000, "1"),
    (0xc000_0000_0000_0000, "-2"),
    (0x3fe0_0000_0000_0000, "0.5"),
    (0x412e_8480_0000_0000, "1000000"),
    (0x3fd3_3333_3333_3334, "0.30000000000000004"),
    (0x444b_1ae4_d6e2_ef50, "1e+21"),
    (0x4454_542b_a12a_337c, "1.5e+21"),
    (0x4415_af1d_78b5_8c40, "100000000000000000000"),
    (0xc415_af1d_78b5_8c40, "-100000000000000000000"),
    (0x441a_c53a_7e04_bcda, "123456789012345680000"),
    (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
    (0x3e7a_d7f2_9abc_af48, "1e-7"),
    (0x3e80_c6f7_a0b5_ed8d, "1.25e-7"),
    (0x3fe0_ca90_d70c_b627, "0.5247272682369769"),
    (0x43b0_0000_0000_0000, "1152921504606847000"),
    (0x43e0_0000_0000_0000, "9223372036854776000"),
    (0xc3e0_0000_0000_0000, "-9223372036854776000"),
    (0x43f0_0000_0000_0000, "18446744073709552000"),
    (0x43e0_2207_973f_6440, "9300000000000000000"),
    (0x430c_6bf5_2634_0000, "1000000000000000"),
    (0x3efa_36e2_eb1c_432d, "0.000025"),
    (0x0000_0000_0000_0001, "5e-324"),
    (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
    (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
    (0x0010_0000_0000_0000, "2.2250738585072014e-308"),
    (0x4340_0000_0000_0000, "9007199254740992"),
    (0x4340_0000_0000_0001, "9007199254740994"),
    (0x4340_0000_0000_0000, "9007199254740992"),
    (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
    (0x47f0_0000_0000_0000, "3.402823669209385e+38"),
    (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
    (0x3eb4_b623_1abf_d271, "0.0000012345"),
    (0x0000_0000_0000_0002, "1e-323"),
    (0x405e_dd2f_1a9f_be77, "123.456"),
    (0x54b2_49ad_2594_c37d, "1e+100"),
    (0x7e41_eb2d_6600_5835, "1.5e+300"),
    (0x4011_6666_6666_6666, "4.35"),
    (0x3fb9_9999_9999_999a, "0.1"),
    (0x4059_0000_0000_0000, "100"),
    (0x7e6d_dd4b_aa00_9303, "1e+301"),
    (0xfe6d_dd4b_aa00_9303, "-1e+301"),
    (0x7ff8_0000_0000_0000, "NaN"),
];

#[test]
fn number_to_string_matches_node_string() {
    let printed: Vec<(u64, String)> = TO_STRING
        .iter()
        .map(|&(bits, _)| (bits, js_number_to_string(f64::from_bits(bits))))
        .collect();
    let expected: Vec<(u64, String)> = TO_STRING
        .iter()
        .map(|&(bits, text)| (bits, text.to_string()))
        .collect();
    assert_eq!(printed, expected);
}
