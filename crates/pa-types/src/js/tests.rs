//! `js_number` against Node's `Number(text)`: the table is node v26 output
//! (regenerate with the inputs below and `Number(s)` printed as f64 bits).

use super::js_number;

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
    ("\t\n\u{b}\u{c}\r \u{a0}\u{1680}\u{2000}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}", G::Bits(0x0000_0000_0000_0000)),
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
    ("0xffffffffffffffffffffffffffffffff", G::Bits(0x47f0_0000_0000_0000)),
    ("0b1111111111111111111111111111111111111111111111111111111111111111111111", G::Bits(0x4450_0000_0000_0000)),
    ("1e999", G::PosInf),
    ("-1e999", G::NegInf),
    ("1e-999", G::Bits(0x0000_0000_0000_0000)),
    ("4.9e-324", G::Bits(0x0000_0000_0000_0001)),
    ("2.4703282292062328e-324", G::Bits(0x0000_0000_0000_0001)),
    ("2.4703282292062327e-324", G::Bits(0x0000_0000_0000_0000)),
    ("0.1", G::Bits(0x3fb9_9999_9999_999a)),
    ("0.30000000000000004", G::Bits(0x3fd3_3333_3333_3334)),
    ("9007199254740993", G::Bits(0x4340_0000_0000_0000)),
    ("123456789012345678901234567890", G::Bits(0x45f8_ee90_ff6c_373e)),
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
