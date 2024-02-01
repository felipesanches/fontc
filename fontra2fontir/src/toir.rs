//! Functions to convert fontra things to fontc IR things

use std::collections::{HashMap, HashSet};

use fontdrasil::{
    coords::{CoordConverter, DesignCoord, NormalizedCoord, NormalizedLocation, UserCoord},
    types::{Axis, GlyphName},
};
use fontir::{
    error::WorkError,
    ir::{Glyph, GlyphInstance, GlyphPathBuilder, StaticMetadata},
};
use kurbo::BezPath;
use log::trace;

use crate::fontra::{FontraContour, FontraFontData, FontraGlyph, FontraPoint, PointType};

pub(crate) fn to_ir_static_metadata(
    font_data: &FontraFontData,
) -> Result<StaticMetadata, WorkError> {
    let axes = font_data
        .axes
        .iter()
        .map(|a| {
            let min = UserCoord::new(a.min_value as f32);
            let default = UserCoord::new(a.default_value as f32);
            let max = UserCoord::new(a.max_value as f32);

            if min > default || max < default {
                return Err(WorkError::InconsistentAxisDefinitions(format!("{a:?}")));
            }

            let converter = if !a.mapping.is_empty() {
                let examples: Vec<_> = a
                    .mapping
                    .iter()
                    .map(|[raw_user, raw_design]| {
                        (
                            UserCoord::new(*raw_user as f32),
                            DesignCoord::new(*raw_design as f32),
                        )
                    })
                    .collect();
                let default_idx = examples
                    .iter()
                    .position(|(u, _)| *u == default)
                    .ok_or_else(|| WorkError::AxisMustMapDefault(a.tag))?;
                examples
                    .iter()
                    .position(|(u, _)| *u == min)
                    .ok_or_else(|| WorkError::AxisMustMapMin(a.tag))?;
                examples
                    .iter()
                    .position(|(u, _)| *u == max)
                    .ok_or_else(|| WorkError::AxisMustMapMax(a.tag))?;
                CoordConverter::new(examples, default_idx)
            } else {
                CoordConverter::unmapped(min, default, max)
            };

            Ok(Axis {
                tag: a.tag,
                name: a.name.clone(),
                hidden: a.hidden,
                min,
                default,
                max,
                converter,
            })
        })
        .collect::<Result<_, _>>()?;

    StaticMetadata::new(
        font_data.units_per_em,
        Default::default(),
        axes,
        Default::default(),
        Default::default(), // TODO: glyph locations we really do need
        Default::default(),
        Default::default(),
    )
    .map_err(WorkError::VariationModelError)
}

#[allow(dead_code)] // TEMPORARY
fn to_ir_glyph(
    default_location: NormalizedLocation,
    codepoints: HashSet<u32>,
    fontra_glyph: &FontraGlyph,
) -> Result<Glyph, WorkError> {
    let layer_locations: HashMap<_, _> = fontra_glyph
        .sources
        .iter()
        .map(|s| {
            let mut location = default_location.clone();
            for (tag, pos) in s.location.iter() {
                if !location.contains(*tag) {
                    return Err(WorkError::UnexpectedAxisPosition(
                        fontra_glyph.name.clone(),
                        tag.to_string(),
                    ));
                }
                location.insert(*tag, NormalizedCoord::new(*pos as f32));
            }
            Ok((s.layer_name.as_str(), location))
        })
        .collect::<Result<_, _>>()?;

    let mut instances = HashMap::new();
    for (layer_name, layer) in fontra_glyph.layers.iter() {
        let Some(location) = layer_locations.get(layer_name.as_str()) else {
            return Err(WorkError::NoSourceForName(layer_name.clone()));
        };

        let contours: Vec<_> = layer
            .glyph
            .path
            .contours
            .iter()
            .map(|c| to_ir_path(fontra_glyph.name.clone(), c))
            .collect::<Result<_, _>>()?;
        instances.insert(
            location.clone(),
            GlyphInstance {
                width: layer.glyph.x_advance,
                contours,
                ..Default::default()
            },
        );
    }

    Glyph::new(fontra_glyph.name.clone(), true, codepoints, instances)
}

#[allow(dead_code)] // TEMPORARY
fn add_to_path<'a>(
    glyph_name: GlyphName,
    path_builder: &'a mut GlyphPathBuilder,
    points: impl Iterator<Item = &'a FontraPoint>,
) -> Result<(), WorkError> {
    // Walk through the remaining points, accumulating off-curve points until we see an on-curve
    // https://github.com/googlefonts/glyphsLib/blob/24b4d340e4c82948ba121dcfe563c1450a8e69c9/Lib/glyphsLib/pens.py#L92
    for point in points {
        let point_type = point
            .point_type()
            .map_err(|e| WorkError::InvalidSourceGlyph {
                glyph_name: glyph_name.clone(),
                message: format!("No point type for {point:?}: {e}"),
            })?;
        // Smooth is only relevant to editors so ignore here
        match point_type {
            PointType::OnCurve | PointType::OnCurveSmooth => path_builder
                .curve_to((point.x, point.y))
                .map_err(WorkError::PathConversionError)?,
            PointType::OffCurveQuad | PointType::OffCurveCubic => path_builder
                .offcurve((point.x, point.y))
                .map_err(WorkError::PathConversionError)?,
        }
    }
    Ok(())
}

fn to_ir_path(glyph_name: GlyphName, contour: &FontraContour) -> Result<BezPath, WorkError> {
    // Based on glyphs2fontir/src/toir.rs to_ir_path
    // TODO: so similar a trait to to let things be added to GlyphPathBuilder would be nice
    if contour.points.is_empty() {
        return Ok(BezPath::new());
    }

    let mut path_builder = GlyphPathBuilder::new(glyph_name.clone(), contour.points.len());

    if !contour.is_closed {
        let first = contour.points.first().unwrap();
        let first_type = first
            .point_type()
            .map_err(|e| WorkError::InvalidSourceGlyph {
                glyph_name: glyph_name.clone(),
                message: format!("No point type for {first:?}: {e}"),
            })?;
        if first_type.is_off_curve() {
            return Err(WorkError::InvalidSourceGlyph {
                glyph_name: glyph_name.clone(),
                message: String::from("Open path starts with off-curve points"),
            });
        }
        path_builder.move_to((first.x, first.y))?;
        add_to_path(
            glyph_name.clone(),
            &mut path_builder,
            contour.points[1..].iter(),
        )?;
    } else {
        add_to_path(glyph_name.clone(), &mut path_builder, contour.points.iter())?;
    }

    let path = path_builder.build()?;
    trace!(
        "Built a {} entry path for {}",
        path.elements().len(),
        glyph_name
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use fontdrasil::{coords::NormalizedCoord, types::Axis};
    use write_fonts::types::Tag;

    use crate::{
        fontra::{FontraFontData, FontraGlyph},
        test::testdata_dir,
        toir::to_ir_static_metadata,
    };

    use super::to_ir_glyph;

    fn axis_tuples(axes: &[Axis]) -> Vec<(&str, Tag, f64, f64, f64)> {
        axes.iter()
            .map(|a| {
                (
                    a.name.as_str(),
                    a.tag,
                    a.min.to_f32() as f64,
                    a.default.to_f32() as f64,
                    a.max.to_f32() as f64,
                )
            })
            .collect::<Vec<_>>()
    }

    #[test]
    fn static_metadata_of_2glyphs() {
        let fontdata_file = testdata_dir().join("2glyphs.fontra/font-data.json");
        let font_data = FontraFontData::from_file(&fontdata_file).unwrap();
        let static_metadata = to_ir_static_metadata(&font_data).unwrap();
        assert_eq!(1000, static_metadata.units_per_em);
        assert_eq!(
            vec![
                ("Weight", Tag::from_be_bytes(*b"wght"), 200.0, 200.0, 900.0),
                ("Width", Tag::from_be_bytes(*b"wdth"), 50.0, 100.0, 125.0)
            ],
            axis_tuples(&static_metadata.axes)
        );
    }

    #[test]
    fn ir_of_glyph_u20089() {
        let default_location =
            vec![(Tag::from_be_bytes(*b"wght"), NormalizedCoord::new(0.0))].into();
        let glyph_file = testdata_dir().join("2glyphs.fontra/glyphs/u20089.json");
        let fontra_glyph = FontraGlyph::from_file(&glyph_file).unwrap();
        let glyph = to_ir_glyph(default_location, Default::default(), &fontra_glyph).unwrap();
        for (l, i) in glyph.sources() {
            for c in i.contours.iter() {
                eprintln!("<path d=\"{}\" opacity=\"0.5\"/>", c.to_svg());
            }
        }
        todo!("check something on {glyph:#?}");
    }
}
