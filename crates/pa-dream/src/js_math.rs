//! `Math.log` and `Math.cos` exactly as Node's V8 computes them, so a seeded
//! rollout draws the same gaussians the TS product drew.
//!
//! Node builds V8 without `V8_USE_LIBM_TRIG_FUNCTIONS`, so both are ports of
//! Sun's original fdlibm (`deps/v8/src/base/ieee754.cc`): `e_log.c`, and
//! `s_cos.c` over `__ieee754_rem_pio2`, `__kernel_cos` and `__kernel_sin`.
//! They round differently from the later FreeBSD/musl versions the `libm`
//! crate ports (about 2% of `log` and 1% of `cos` results differ by an ulp)
//! and from glibc's. `cos` reproduces V8 bit for bit for `|x| <= 2^19·π/2`;
//! beyond that it falls back to the platform `cos`.
//!
//! The constants keep fdlibm's decimal spelling digit for digit (they round to
//! the same doubles), and the bodies keep its single-letter names, so the port
//! reads line for line against `ieee754.cc`.
#![allow(
    clippy::excessive_precision,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::unreadable_literal
)]

fn high_word(x: f64) -> i32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let word = (x.to_bits() >> 32) as i32;
    word
}

fn low_word(x: f64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let word = x.to_bits() as u32;
    word
}

fn with_high_word(x: f64, high: i32) -> f64 {
    #[allow(clippy::cast_sign_loss)]
    let high = u64::from(high as u32);
    f64::from_bits((high << 32) | u64::from(low_word(x)))
}

/// V8 `base::ieee754::log` (fdlibm `e_log.c`).
#[must_use]
pub fn log(mut x: f64) -> f64 {
    const LN2_HI: f64 = 6.93147180369123816490e-01;
    const LN2_LO: f64 = 1.90821492927058770002e-10;
    const TWO54: f64 = 1.80143985094819840000e+16;
    const LG1: f64 = 6.666666666666735130e-01;
    const LG2: f64 = 3.999999999940941908e-01;
    const LG3: f64 = 2.857142874366239149e-01;
    const LG4: f64 = 2.222219843214978396e-01;
    const LG5: f64 = 1.818357216161805012e-01;
    const LG6: f64 = 1.531383769920937332e-01;
    const LG7: f64 = 1.479819860511658591e-01;

    let mut hx = high_word(x);
    let lx = low_word(x);
    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        if ((hx & 0x7FFF_FFFF).cast_unsigned() | lx) == 0 {
            return f64::NEG_INFINITY;
        }
        if hx < 0 {
            return f64::NAN;
        }
        k -= 54;
        x *= TWO54;
        hx = high_word(x);
    }
    if hx >= 0x7FF0_0000 {
        return x + x;
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000F_FFFF;
    let i = (hx + 0x95F64) & 0x10_0000;
    x = with_high_word(x, hx | (i ^ 0x3FF0_0000));
    k += i >> 20;
    let f = x - 1.0;
    if (0x000F_FFFF & (2 + hx)) < 3 {
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = f64::from(k);
            return dk * LN2_HI + dk * LN2_LO;
        }
        let r = f * f * (0.5 - 0.333_333_333_333_333_33 * f);
        if k == 0 {
            return f - r;
        }
        let dk = f64::from(k);
        return dk * LN2_HI - ((r - dk * LN2_LO) - f);
    }
    let s = f / (2.0 + f);
    let dk = f64::from(k);
    let z = s * s;
    let mut i = hx - 0x6147A;
    let w = z * z;
    let j = 0x6B851 - hx;
    let t1 = w * (LG2 + w * (LG4 + w * LG6));
    let t2 = z * (LG1 + w * (LG3 + w * (LG5 + w * LG7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * LN2_HI - ((hfsq - (s * (hfsq + r) + dk * LN2_LO)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * LN2_HI - ((s * (f - r) - dk * LN2_LO) - f)
    }
}

/// V8 `__ieee754_rem_pio2` for `|x| <= 2^19 · π/2`: `x = n·π/2 + y0 + y1`.
fn rem_pio2_medium(x: f64) -> (i32, f64, f64) {
    const NPIO2_HW: [i32; 32] = [
        0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB,
        0x402921FB, 0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB,
        0x40378FDB, 0x403921FB, 0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C,
        0x4041475C, 0x4042106C, 0x4042D97C, 0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB,
        0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
    ];
    // fdlibm's 53 bits of 2/pi, spelled as `ieee754.cc` spells them.
    #[allow(clippy::approx_constant)]
    const INVPIO2: f64 = 6.36619772367581382433e-01;
    const PIO2_1: f64 = 1.57079632673412561417e+00;
    const PIO2_1T: f64 = 6.07710050650619224932e-11;
    const PIO2_2: f64 = 6.07710050630396597660e-11;
    const PIO2_2T: f64 = 2.02226624879595063154e-21;
    const PIO2_3: f64 = 2.02226624871116645580e-21;
    const PIO2_3T: f64 = 8.47842766036889956997e-32;

    let hx = high_word(x);
    let ix = hx & 0x7FFF_FFFF;
    if ix <= 0x3FE9_21FB {
        return (0, x, 0.0);
    }
    if ix < 0x4002_D97C {
        return if hx > 0 {
            let mut z = x - PIO2_1;
            if ix == 0x3FF9_21FB {
                z -= PIO2_2;
                let y0 = z - PIO2_2T;
                (1, y0, (z - y0) - PIO2_2T)
            } else {
                let y0 = z - PIO2_1T;
                (1, y0, (z - y0) - PIO2_1T)
            }
        } else {
            let mut z = x + PIO2_1;
            if ix == 0x3FF9_21FB {
                z += PIO2_2;
                let y0 = z + PIO2_2T;
                (-1, y0, (z - y0) + PIO2_2T)
            } else {
                let y0 = z + PIO2_1T;
                (-1, y0, (z - y0) + PIO2_1T)
            }
        };
    }
    let t = x.abs();
    #[allow(clippy::cast_possible_truncation)]
    let n = (t * INVPIO2 + 0.5) as i32;
    let fn_ = f64::from(n);
    let mut r = t - fn_ * PIO2_1;
    let mut w = fn_ * PIO2_1T;
    let mut y0;
    if n < 32 && ix != NPIO2_HW[usize::try_from(n - 1).unwrap_or(0)] {
        y0 = r - w;
    } else {
        let j = ix >> 20;
        y0 = r - w;
        let mut i = j - ((high_word(y0) >> 20) & 0x7FF);
        if i > 16 {
            let t = r;
            w = fn_ * PIO2_2;
            r = t - w;
            w = fn_ * PIO2_2T - ((t - r) - w);
            y0 = r - w;
            i = j - ((high_word(y0) >> 20) & 0x7FF);
            if i > 49 {
                let t = r;
                w = fn_ * PIO2_3;
                r = t - w;
                w = fn_ * PIO2_3T - ((t - r) - w);
                y0 = r - w;
            }
        }
    }
    let y1 = (r - y0) - w;
    if hx < 0 {
        (-n, -y0, -y1)
    } else {
        (n, y0, y1)
    }
}

/// V8 `__kernel_cos`.
fn kernel_cos(x: f64, y: f64) -> f64 {
    const C1: f64 = 4.16666666666666019037e-02;
    const C2: f64 = -1.38888888888741095749e-03;
    const C3: f64 = 2.48015872894767294178e-05;
    const C4: f64 = -2.75573143513906633035e-07;
    const C5: f64 = 2.08757232129817482790e-09;
    const C6: f64 = -1.13596475577881948265e-11;
    let ix = high_word(x) & 0x7FFF_FFFF;
    #[allow(clippy::cast_possible_truncation)]
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return 1.0;
    }
    let z = x * x;
    let r = z * (C1 + z * (C2 + z * (C3 + z * (C4 + z * (C5 + z * C6)))));
    if ix < 0x3FD3_3333 {
        return 1.0 - (0.5 * z - (z * r - x * y));
    }
    let qx = if ix > 0x3FE9_0000 {
        0.28125
    } else {
        #[allow(clippy::cast_sign_loss)]
        let high = u64::from((ix - 0x0020_0000) as u32);
        f64::from_bits(high << 32)
    };
    let iz = 0.5 * z - qx;
    let a = 1.0 - qx;
    a - (iz - (z * r - x * y))
}

/// V8 `__kernel_sin` with `iy = 1`.
fn kernel_sin(x: f64, y: f64) -> f64 {
    const S1: f64 = -1.66666666666666324348e-01;
    const S2: f64 = 8.33333333332248946124e-03;
    const S3: f64 = -1.98412698298579493134e-04;
    const S4: f64 = 2.75573137070700676789e-06;
    const S5: f64 = -2.50507602534068634195e-08;
    const S6: f64 = 1.58969099521155010221e-10;
    let ix = high_word(x) & 0x7FFF_FFFF;
    #[allow(clippy::cast_possible_truncation)]
    if ix < 0x3E40_0000 && x as i32 == 0 {
        return x;
    }
    let z = x * x;
    let v = z * x;
    let r = S2 + z * (S3 + z * (S4 + z * (S5 + z * S6)));
    x - ((z * (0.5 * y - v * r) - y) - v * S1)
}

/// V8 `Math.cos` (fdlibm `s_cos.c`).
#[must_use]
pub fn cos(x: f64) -> f64 {
    let ix = high_word(x) & 0x7FFF_FFFF;
    if ix <= 0x3FE9_21FB {
        return kernel_cos(x, 0.0);
    }
    if !x.is_finite() {
        // cos(Inf or NaN) is NaN.
        return f64::NAN;
    }
    if ix > 0x4139_21FB {
        // Beyond 2^19·π/2 fdlibm reduces with `__kernel_rem_pio2`; the dream
        // core never gets there (it only takes `cos(2π·u)`, `u < 1`).
        return x.cos();
    }
    let (n, y0, y1) = rem_pio2_medium(x);
    match n & 3 {
        0 => kernel_cos(y0, y1),
        1 => -kernel_sin(y0, y1),
        2 => -kernel_cos(y0, y1),
        _ => kernel_sin(y0, y1),
    }
}
