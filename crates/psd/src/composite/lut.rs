//! Three-dimensional colour lookup tables for Color Lookup adjustment layers.
//!
//! A layer embeds the table file it was made from. Only the Iridas `.cube`
//! text format is read here; other embedded formats leave the layer unrendered.

/// A cubic lookup table over an input domain, red index varying fastest as in
/// the `.cube` format.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Lut3d {
    size: usize,
    domain_min: [f32; 3],
    domain_max: [f32; 3],
    table: Vec<[f32; 3]>,
}

impl Lut3d {
    /// Parse a 3D `.cube` file. `None` for a 1D table, a malformed file or
    /// one whose entry count does not match its declared size.
    pub fn parse_cube(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut size = None;
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];
        let mut table = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut words = line.split_whitespace();
            let first = words.next()?;
            match first {
                "TITLE" => {}
                "LUT_1D_SIZE" | "LUT_1D_INPUT_RANGE" => return None,
                "LUT_3D_SIZE" => size = Some(words.next()?.parse::<usize>().ok()?),
                "DOMAIN_MIN" => domain_min = parse_triple(words)?,
                "DOMAIN_MAX" => domain_max = parse_triple(words)?,
                _ => {
                    let red = first
                        .parse::<f32>()
                        .ok()
                        .filter(|value| value.is_finite())?;
                    let [green, blue] = parse_pair(words)?;
                    table.push([red, green, blue]);
                }
            }
        }
        let size = size.filter(|size| (2..=256).contains(size))?;
        if table.len() != size * size * size
            || (0..3).any(|axis| domain_max[axis] <= domain_min[axis])
        {
            return None;
        }
        Some(Self {
            size,
            domain_min,
            domain_max,
            table,
        })
    }

    fn at(&self, red: usize, green: usize, blue: usize) -> [f32; 3] {
        self.table[red + self.size * (green + self.size * blue)]
    }

    /// Integer documents use tetrahedral interpolation; float documents use
    /// trilinear interpolation. Outputs are clamped to the unit range.
    ///
    /// Approximation: table precision and integer rounding are continuous here,
    /// and dithering is not implemented.
    pub fn eval(&self, color: [f32; 3], depth: u16) -> [f32; 3] {
        let last = (self.size - 1) as f32;
        let mut base = [0usize; 3];
        let mut fraction = [0.0f32; 3];
        for axis in 0..3 {
            let unit = (color[axis] - self.domain_min[axis])
                / (self.domain_max[axis] - self.domain_min[axis]);
            let position = unit.clamp(0.0, 1.0) * last;
            // Keep the last cell addressable at the top of the domain.
            base[axis] = (position.floor() as usize).min(self.size - 2);
            fraction[axis] = position - base[axis] as f32;
        }
        let [r, g, b] = base;
        let corner = |dr: usize, dg: usize, db: usize| self.at(r + dr, g + dg, b + db);
        let [fr, fg, fb] = fraction;
        if depth == 32 {
            let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
                std::array::from_fn(|channel| a[channel] + t * (b[channel] - a[channel]))
            };
            let near = lerp(
                lerp(corner(0, 0, 0), corner(1, 0, 0), fr),
                lerp(corner(0, 1, 0), corner(1, 1, 0), fr),
                fg,
            );
            let far = lerp(
                lerp(corner(0, 0, 1), corner(1, 0, 1), fr),
                lerp(corner(0, 1, 1), corner(1, 1, 1), fr),
                fg,
            );
            return lerp(near, far, fb).map(|value| value.clamp(0.0, 1.0));
        }
        // Walk from the origin corner to the opposite one along the axes in
        // order of decreasing fraction; the path picks the enclosing tetrahedron.
        let mut axes = [(fr, 0usize), (fg, 1), (fb, 2)];
        axes.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut step = [0usize; 3];
        let mut previous = corner(0, 0, 0);
        let mut out = previous;
        for (weight, axis) in axes {
            step[axis] = 1;
            let next = corner(step[0], step[1], step[2]);
            for channel in 0..3 {
                out[channel] += weight * (next[channel] - previous[channel]);
            }
            previous = next;
        }
        out.map(|value| value.clamp(0.0, 1.0))
    }
}

fn parse_triple<'a>(words: impl Iterator<Item = &'a str>) -> Option<[f32; 3]> {
    let mut values = words.map(|word| word.parse::<f32>().ok().filter(|value| value.is_finite()));
    let triple = [values.next()??, values.next()??, values.next()??];
    values.next().is_none().then_some(triple)
}

fn parse_pair<'a>(words: impl Iterator<Item = &'a str>) -> Option<[f32; 2]> {
    let mut values = words.map(|word| word.parse::<f32>().ok().filter(|value| value.is_finite()));
    let pair = [values.next()??, values.next()??];
    values.next().is_none().then_some(pair)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: &[u8] =
        b"TITLE \"id\"\nLUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n";

    #[test]
    fn an_identity_cube_returns_its_input() {
        let lut = Lut3d::parse_cube(IDENTITY).unwrap();
        for color in [
            [0.0, 0.0, 0.0],
            [0.2, 0.7, 0.4],
            [1.0, 0.5, 0.0],
            [0.9, 0.9, 0.1],
        ] {
            let out = lut.eval(color, 8);
            for channel in 0..3 {
                assert!(
                    (out[channel] - color[channel]).abs() < 1e-6,
                    "{color:?} {out:?}"
                );
            }
        }
    }

    #[test]
    fn red_varies_fastest_and_outputs_follow_the_table() {
        // Swap red and blue in the output.
        let swapped = b"LUT_3D_SIZE 2\n0 0 0\n0 0 1\n0 1 0\n0 1 1\n1 0 0\n1 0 1\n1 1 0\n1 1 1\n";
        let lut = Lut3d::parse_cube(swapped).unwrap();
        let out = lut.eval([1.0, 0.0, 0.0], 8);
        assert_eq!(out, [0.0, 0.0, 1.0]);
        let out = lut.eval([0.25, 0.5, 0.75], 8);
        assert!(
            (out[0] - 0.75).abs() < 1e-6 && (out[2] - 0.25).abs() < 1e-6,
            "{out:?}"
        );
    }

    #[test]
    fn a_custom_domain_rescales_the_input() {
        let text = String::from_utf8_lossy(IDENTITY).replace(
            "LUT_3D_SIZE 2",
            "LUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\nDOMAIN_MAX 2 2 2",
        );
        let lut = Lut3d::parse_cube(text.as_bytes()).unwrap();
        let out = lut.eval([1.0, 1.0, 1.0], 8);
        assert!(
            out.iter().all(|value| (value - 0.5).abs() < 1e-6),
            "{out:?}"
        );
    }

    #[test]
    fn malformed_or_one_dimensional_tables_are_rejected() {
        assert!(Lut3d::parse_cube(b"LUT_1D_SIZE 4\n0 0 0\n").is_none());
        assert!(Lut3d::parse_cube(b"LUT_3D_SIZE 2\n0 0 0\n").is_none());
        assert!(Lut3d::parse_cube(b"not a table").is_none());
        for text in [
            "DOMAIN_MIN NaN 0 0",
            "DOMAIN_MAX 1 inf 1",
            "DOMAIN_MIN 0 0 0 1",
        ] {
            let cube = format!("{text}\n{}", String::from_utf8_lossy(IDENTITY));
            assert!(Lut3d::parse_cube(cube.as_bytes()).is_none());
        }
        for triple in ["NaN 0 0", "0 inf 0", "0 0 NaN", "0 0 0 1"] {
            let cube = String::from_utf8_lossy(IDENTITY).replacen("0 0 0", triple, 1);
            assert!(Lut3d::parse_cube(cube.as_bytes()).is_none());
        }
    }

    #[test]
    fn nonlinear_tables_use_trilinear_interpolation_only_for_float_documents() {
        // Only the white corner is nonzero: tetrahedral interpolation takes
        // the smallest fraction, whereas trilinear multiplies all three.
        let cube = b"LUT_3D_SIZE 2\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n0 0 0\n1 1 1\n";
        let lut = Lut3d::parse_cube(cube).unwrap();
        for color in [[0.2, 0.5, 0.8], [0.8, 0.2, 0.5], [0.5, 0.8, 0.2], [0.5; 3]] {
            let minimum = color.into_iter().fold(1.0f32, f32::min);
            for depth in [8, 16] {
                assert_eq!(lut.eval(color, depth), [minimum; 3]);
            }
            let product = color.into_iter().product::<f32>();
            assert!(lut
                .eval(color, 32)
                .iter()
                .all(|value| (value - product).abs() < 1e-6));
        }
        assert_eq!(lut.eval([1.0; 3], 32), [1.0; 3]);
        assert_eq!(lut.eval([0.0; 3], 32), [0.0; 3]);
    }
}
