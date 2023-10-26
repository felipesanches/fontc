//! Feature binary compilation.

use std::{
    collections::{HashMap, HashSet},
    error::Error as StdError,
    ffi::{OsStr, OsString},
    fmt::Display,
    fs,
    path::PathBuf,
    sync::Arc,
};

use fea_rs::{
    compile::{Compilation, VariationInfo},
    parse::{SourceLoadError, SourceResolver},
    Compiler, GlyphMap, GlyphName as FeaRsGlyphName,
};
use font_types::Tag;
use fontdrasil::{coords::NormalizedLocation, types::Axis};
use fontir::{
    ir::{Features, GlyphOrder, KernParticipant, Kerning, StaticMetadata},
    orchestration::{Flags, WorkId as FeWorkId},
};
use log::{debug, error, trace, warn};

use fontdrasil::orchestration::{Access, Work};
use write_fonts::OtRound;

use crate::{
    error::Error,
    orchestration::{AnyWorkId, BeWork, Context, WorkId},
};

#[derive(Debug)]
pub struct FeatureWork {}

// I did not want to make a struct
// I did not want to clone the content
// I do not like this construct
// I do find the need to lament
struct InMemoryResolver {
    content_path: OsString,
    content: Arc<str>,
    // Our fea might be generated in memory, such as to inject generated kerning,
    // while compiling a disk-based source with a well defined include path
    include_dir: Option<PathBuf>,
}

impl SourceResolver for InMemoryResolver {
    fn get_contents(&self, rel_path: &OsStr) -> Result<Arc<str>, SourceLoadError> {
        if rel_path == &*self.content_path {
            return Ok(self.content.clone());
        }
        let Some(include_dir) = &self.include_dir else {
            return Err(SourceLoadError::new(
                rel_path.to_os_string(),
                NoIncludePathError::new(),
            ));
        };
        let path = include_dir
            .join(rel_path)
            .canonicalize()
            .map_err(|e| SourceLoadError::new(rel_path.to_os_string(), e))?;
        if !path.is_file() {
            return Err(SourceLoadError::new(
                rel_path.to_os_string(),
                Error::FileExpected(path),
            ));
        }
        trace!("Resolved {rel_path:?} to {path:?}");
        let contents = fs::read_to_string(path)
            .map_err(|e| SourceLoadError::new(rel_path.to_os_string(), e))?;
        Ok(Arc::from(contents.as_str()))
    }
}

#[derive(Debug)]
struct NoIncludePathError {}

impl NoIncludePathError {
    fn new() -> NoIncludePathError {
        NoIncludePathError {}
    }
}

impl std::error::Error for NoIncludePathError {}

impl Display for NoIncludePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("No include path available")?;
        Ok(())
    }
}

struct FeaVariationInfo<'a> {
    axes: HashMap<Tag, (usize, &'a Axis)>,
    static_metadata: &'a StaticMetadata,
}

impl<'a> FeaVariationInfo<'a> {
    fn new(static_metadata: &'a StaticMetadata) -> FeaVariationInfo<'a> {
        FeaVariationInfo {
            axes: static_metadata
                .axes
                .iter()
                .enumerate()
                .map(|(i, a)| (a.tag, (i, a)))
                .collect(),
            static_metadata,
        }
    }
}

#[derive(Debug)]
struct UnsupportedLocationError(NormalizedLocation);

impl UnsupportedLocationError {
    fn new(loc: NormalizedLocation) -> UnsupportedLocationError {
        UnsupportedLocationError(loc)
    }
}

impl std::error::Error for UnsupportedLocationError {}

impl Display for UnsupportedLocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "No variation model for {:?}", self.0)
    }
}

#[derive(Debug)]
struct MissingTentError(Tag);

impl MissingTentError {
    fn new(tag: Tag) -> MissingTentError {
        MissingTentError(tag)
    }
}

impl std::error::Error for MissingTentError {}

impl Display for MissingTentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Missing a tent for {}", self.0)
    }
}

impl<'a> VariationInfo for FeaVariationInfo<'a> {
    fn axis(&self, axis_tag: font_types::Tag) -> Option<(usize, &Axis)> {
        self.axes.get(&axis_tag).map(|(i, a)| (*i, *a))
    }

    fn resolve_variable_metric(
        &self,
        values: &HashMap<NormalizedLocation, i16>,
    ) -> Result<
        (
            i16,
            Vec<(write_fonts::tables::variations::VariationRegion, i16)>,
        ),
        Box<(dyn StdError + 'static)>,
    > {
        let var_model = &self.static_metadata.variation_model;

        // Compute deltas using f64 as 1d point and delta, then ship them home as i16
        let point_seqs: HashMap<_, _> = values
            .iter()
            .map(|(pos, value)| (pos.clone(), vec![*value as f64]))
            .collect();

        // We only support use when the point seq is at a location our variation model supports
        // TODO: get a model for the location we are asked for so we can support sparseness
        for loc in point_seqs.keys() {
            if !var_model.supports(loc) {
                return Err(Box::new(UnsupportedLocationError::new(loc.clone())));
            }
        }

        // Only 1 value per region for our input
        let deltas: Vec<_> = var_model
            .deltas(&point_seqs)?
            .into_iter()
            .map(|(region, values)| {
                assert!(values.len() == 1, "{} values?!", values.len());
                (region, values[0])
            })
            .collect();

        // Compute the default on the unrounded deltas
        let default_value = deltas
            .iter()
            .filter_map(|(region, value)| {
                let scaler = region.scalar_at(&var_model.default).into_inner();
                match scaler {
                    scaler if scaler == 0.0 => None,
                    scaler => Some(scaler * *value as f32),
                }
            })
            .sum::<f32>()
            .ot_round();

        // Produce the desired delta type
        let mut fears_deltas = Vec::with_capacity(deltas.len());
        for (region, value) in deltas.iter().filter(|(r, _)| !r.is_default()) {
            // https://learn.microsoft.com/en-us/typography/opentype/spec/otvarcommonformats#variation-regions
            // Array of region axis coordinates records, in the order of axes given in the 'fvar' table.
            let mut region_axes = Vec::with_capacity(self.static_metadata.axes.len());
            for axis in self.static_metadata.axes.iter() {
                let Some(tent) = region.get(&axis.tag) else {
                    return Err(Box::new(MissingTentError::new(axis.tag)));
                };
                region_axes.push(tent.to_region_axis_coords());
            }
            fears_deltas.push((
                write_fonts::tables::variations::VariationRegion { region_axes },
                value.ot_round(),
            ));
        }

        Ok((default_value, fears_deltas))
    }
}

impl FeatureWork {
    pub fn create() -> Box<BeWork> {
        Box::new(FeatureWork {})
    }

    fn compile(
        &self,
        static_metadata: &StaticMetadata,
        features: &Features,
        glyph_order: GlyphMap,
    ) -> Result<Compilation, Error> {
        let var_info = FeaVariationInfo::new(static_metadata);
        let compiler = match features {
            Features::File {
                fea_file,
                include_dir,
            } => {
                let mut compiler = Compiler::new(OsString::from(fea_file), &glyph_order);
                if let Some(include_dir) = include_dir {
                    compiler = compiler.with_project_root(include_dir)
                }
                compiler
            }
            Features::Memory {
                fea_content,
                include_dir,
            } => {
                let root = OsString::new();
                let mut compiler =
                    Compiler::new(root.clone(), &glyph_order).with_resolver(InMemoryResolver {
                        content_path: root,
                        content: Arc::from(fea_content.as_str()),
                        include_dir: include_dir.clone(),
                    });
                if let Some(include_dir) = include_dir {
                    compiler = compiler.with_project_root(include_dir)
                }
                compiler
            }
            Features::Empty => panic!("compile isn't supposed to be called for Empty"),
        }
        .with_variable_info(&var_info);
        compiler.compile().map_err(Error::FeaCompileError)
    }
}

fn write_debug_fea(context: &Context, is_error: bool, why: &str, fea_content: &str) {
    if !context.flags.contains(Flags::EMIT_DEBUG) {
        if is_error {
            warn!("Debug fea not written for '{why}' because --emit_debug is off");
        }
        return;
    }
    let debug_file = context.debug_dir().join("features.fea");
    match fs::write(&debug_file, fea_content) {
        Ok(..) => {
            if is_error {
                warn!("{}; fea written to {:?}", why, debug_file)
            } else {
                debug!("fea written to {:?}", debug_file);
            }
        }
        Err(e) => error!("{}; failed to write fea to {:?}: {}", why, debug_file, e),
    };
}

fn create_glyphmap(glyph_order: &GlyphOrder) -> GlyphMap {
    if glyph_order.is_empty() {
        warn!("Glyph order is empty; feature compile improbable");
    }
    glyph_order
        .iter()
        .map(|n| Into::<FeaRsGlyphName>::into(n.as_str()))
        .collect()
}

fn push_identifier(fea: &mut String, identifier: &KernParticipant) {
    match identifier {
        KernParticipant::Glyph(name) => fea.push_str(name.as_str()),
        KernParticipant::Group(name) => {
            fea.push('@');
            fea.push_str(name.as_str());
        }
    }
}

#[inline]
fn enumerated(kp1: &KernParticipant, kp2: &KernParticipant) -> bool {
    // Glyph to class or class to glyph pairs are interpreted as 'class exceptions' to class pairs
    // and are thus prefixed with 'enum' keyword so they will be enumerated as specific glyph-glyph pairs.
    // http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#6bii-enumerating-pairs
    // https://github.com/googlefonts/ufo2ft/blob/b3895a9/Lib/ufo2ft/featureWriters/kernFeatureWriter.py#L360
    kp1.is_group() ^ kp2.is_group()
}

/// Create a single variable fea describing the kerning for the entire variation space.
///
/// No merge baby! - [context](https://github.com/fonttools/fonttools/issues/3168#issuecomment-1608787520)
///
/// To match existing behavior, all kerns must have values for all locations for which any kerning is specified.
/// See <https://github.com/fonttools/fonttools/issues/3168#issuecomment-1603631080> for more.
/// Missing values are populated using the <https://unifiedfontobject.org/versions/ufo3/kerning.plist/#kerning-value-lookup-algorithm>.
/// In future it is likely sparse kerning - blanks filled by interpolation - will be permitted.
///
/// * See <https://github.com/fonttools/fonttools/issues/3168> wrt sparse kerning.
/// * See <https://github.com/adobe-type-tools/afdko/pull/1350> wrt variable fea.
fn create_kerning_fea(kerning: &Kerning) -> Result<String, Error> {
    // Every kern must be defined at these locations. For human readability lets order things consistently.
    let kerned_locations: HashSet<_> = kerning.kerns.values().flat_map(|v| v.keys()).collect();
    let mut kerned_locations: Vec<_> = kerned_locations.into_iter().collect();
    kerned_locations.sort();

    if log::log_enabled!(log::Level::Trace) {
        trace!(
            "The following {} locations have kerning:",
            kerned_locations.len()
        );
        for pos in kerned_locations.iter() {
            trace!("  {pos:?}");
        }
    }

    // For any kern that is incompletely specified fill in the missing values using UFO kerning lookup
    // Not 100% sure if this is correct for .glyphs but lets start there
    // Generate variable format kerning per https://github.com/adobe-type-tools/afdko/pull/1350
    // Use design values per discussion on https://github.com/harfbuzz/boring-expansion-spec/issues/94
    let mut fea = String::new();
    fea.reserve(8192); // TODO is this a good value?
    fea.push_str("\n\n# fontc generated kerning\n\n");

    if kerning.is_empty() {
        return Ok(fea);
    }

    // TODO eliminate singleton groups, e.g. @public.kern1.Je-cy = [Je-cy];

    // 1) Generate classes (http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#2.g.ii)
    // @classname = [glyph1 glyph2 glyph3];
    for (name, members) in kerning.groups.iter() {
        fea.push('@');
        fea.push_str(name.as_str());
        fea.push_str(" = [");
        for member in members {
            fea.push_str(member.as_str());
            fea.push(' ');
        }
        fea.remove(fea.len() - 1);
        fea.push_str("];\n");
    }
    fea.push_str("\n\n");

    // 2) Generate pairpos (http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#6.b)
    // it's likely that many kerns use the same location string, might as well remember the string edition

    let mut pos_strings = HashMap::new();
    fea.push_str("feature kern {\n");
    for ((participant1, participant2), values) in kerning.kerns.iter() {
        fea.push_str("  ");
        if enumerated(participant1, participant2) {
            fea.push_str("enum ");
        }
        fea.push_str("pos ");
        push_identifier(&mut fea, participant1);
        fea.push(' ');
        push_identifier(&mut fea, participant2);

        // See https://github.com/adobe-type-tools/afdko/pull/1350#issuecomment-845219109 for syntax
        // <value>n for normalized, per https://github.com/harfbuzz/boring-expansion-spec/issues/94#issuecomment-1608007111
        fea.push_str(" (");
        for location in kerned_locations.iter() {
            // TODO can we skip some values by dropping where value == interpolated value?
            let advance_adjustment = values
                .get(location)
                .map(|f| f.into_inner())
                // TODO: kerning lookup
                .unwrap_or_else(|| 0.0);

            let pos_str = pos_strings.entry(*location).or_insert_with(|| {
                location
                    .iter()
                    .map(|(tag, value)| format!("{tag}={}n", value.into_inner()))
                    .collect::<Vec<_>>()
                    .join(",")
            });

            fea.push_str(pos_str);
            fea.push(':');
            fea.push_str(&format!("{} ", advance_adjustment));
        }
        fea.remove(fea.len() - 1);
        fea.push_str(");\n");
    }
    fea.push_str("} kern;\n");

    Ok(fea)
}

fn integrate_kerning(features: &Features, kern_fea: String) -> Result<Features, Error> {
    // TODO: insert at proper spot, there's a magic marker that might be present
    match features {
        Features::Empty => Ok(Features::Memory {
            fea_content: kern_fea,
            include_dir: None,
        }),
        Features::Memory {
            fea_content,
            include_dir,
        } => Ok(Features::Memory {
            fea_content: format!("{fea_content}{kern_fea}"),
            include_dir: include_dir.clone(),
        }),
        Features::File {
            fea_file,
            include_dir,
        } => {
            let fea_content = fs::read_to_string(fea_file).map_err(Error::IoError)?;
            Ok(Features::Memory {
                fea_content: format!("{fea_content}{kern_fea}"),
                include_dir: include_dir.clone(),
            })
        }
    }
}

impl Work<Context, AnyWorkId, Error> for FeatureWork {
    fn id(&self) -> AnyWorkId {
        WorkId::Features.into()
    }

    fn read_access(&self) -> Access<AnyWorkId> {
        Access::Set(HashSet::from([
            AnyWorkId::Fe(FeWorkId::GlyphOrder),
            AnyWorkId::Fe(FeWorkId::StaticMetadata),
            AnyWorkId::Fe(FeWorkId::Kerning),
            AnyWorkId::Fe(FeWorkId::Features),
        ]))
    }

    fn also_completes(&self) -> Vec<AnyWorkId> {
        vec![
            WorkId::Gpos.into(),
            WorkId::Gsub.into(),
            WorkId::Gdef.into(),
        ]
    }

    fn exec(&self, context: &Context) -> Result<(), Error> {
        let static_metadata = context.ir.static_metadata.get();
        let glyph_order = context.ir.glyph_order.get();
        let kerning = context.ir.kerning.get();

        let features = if !kerning.is_empty() {
            let kern_fea = create_kerning_fea(&kerning)?;
            integrate_kerning(&context.ir.features.get(), kern_fea)?
        } else {
            (*context.ir.features.get()).clone()
        };

        if !matches!(features, Features::Empty) {
            if log::log_enabled!(log::Level::Trace) {
                if let Features::Memory { fea_content, .. } = &features {
                    trace!("in-memory fea content:\n{fea_content}");
                }
            }

            let glyph_map = create_glyphmap(glyph_order.as_ref());
            let result = self.compile(&static_metadata, &features, glyph_map);
            if result.is_err() || context.flags.contains(Flags::EMIT_DEBUG) {
                if let Features::Memory { fea_content, .. } = &features {
                    write_debug_fea(context, result.is_err(), "compile failed", fea_content);
                }
            }
            let result = result?;

            debug!(
                "Built features, gpos? {} gsub? {} gdef? {}",
                result.gpos.is_some(),
                result.gsub.is_some(),
                result.gdef.is_some(),
            );
            if let Some(gpos) = result.gpos {
                context.gpos.set_unconditionally(gpos.into());
            }
            if let Some(gsub) = result.gsub {
                context.gsub.set_unconditionally(gsub.into());
            }
            if let Some(gdef) = result.gdef {
                context.gdef.set_unconditionally(gdef.into());
            }
        } else {
            debug!("No fea file, dull compile");
        }

        // Enables the assumption that if the file exists features were compiled
        if context.flags.contains(Flags::EMIT_IR) {
            fs::write(
                context
                    .persistent_storage
                    .paths
                    .target_file(&WorkId::Features),
                "1",
            )
            .map_err(Error::IoError)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use fea_rs::compile::VariationInfo;
    use font_types::Tag;
    use fontdrasil::{
        coords::{CoordConverter, DesignCoord, NormalizedCoord, UserCoord},
        types::Axis,
    };
    use fontir::ir::StaticMetadata;

    use super::FeaVariationInfo;

    fn weight_variable_static_metadata(min: f32, def: f32, max: f32) -> StaticMetadata {
        let min_wght_user = UserCoord::new(min);
        let def_wght_user = UserCoord::new(def);
        let max_wght_user = UserCoord::new(max);
        let wght = Tag::new(b"wght");
        let min_wght = vec![(wght, NormalizedCoord::new(-1.0))].into();
        let def_wght = vec![(wght, NormalizedCoord::new(0.0))].into();
        let max_wght = vec![(wght, NormalizedCoord::new(1.0))].into();
        StaticMetadata::new(
            1024,
            Default::default(),
            vec![
                Axis {
                    name: "Weight".to_string(),
                    tag: Tag::new(b"wght"),
                    min: min_wght_user,
                    default: def_wght_user,
                    max: max_wght_user,
                    hidden: false,
                    converter: CoordConverter::new(
                        vec![
                            // the design values don't really matter
                            (min_wght_user, DesignCoord::new(0.0)),
                            (def_wght_user, DesignCoord::new(1.0)),
                            (max_wght_user, DesignCoord::new(2.0)),
                        ],
                        1,
                    ),
                },
                // no-op 'point' axis, should be ignored
                Axis {
                    name: "Width".to_string(),
                    tag: Tag::new(b"wdth"),
                    min: UserCoord::new(0.0),
                    default: UserCoord::new(0.0),
                    max: UserCoord::new(0.0),
                    hidden: false,
                    converter: CoordConverter::new(vec![], 0),
                },
            ],
            Default::default(),
            HashSet::from([min_wght, def_wght, max_wght]),
            Default::default(),
        )
        .unwrap()
    }

    fn is_default(region: &write_fonts::tables::variations::VariationRegion) -> bool {
        region.region_axes.iter().all(|axis_coords| {
            axis_coords.start_coord.to_f32() == 0.0
                && axis_coords.peak_coord.to_f32() == 0.0
                && axis_coords.end_coord.to_f32() == 0.0
        })
    }

    #[test]
    fn resolve_kern() {
        let _ = env_logger::builder().is_test(true).try_init();
        let wght = Tag::new(b"wght");
        let static_metadata = weight_variable_static_metadata(300.0, 400.0, 700.0);
        let var_info = FeaVariationInfo::new(&static_metadata);

        let (default, regions) = var_info
            .resolve_variable_metric(&HashMap::from([
                (vec![(wght, NormalizedCoord::new(-1.0))].into(), 10),
                (vec![(wght, NormalizedCoord::new(0.0))].into(), 15),
                (vec![(wght, NormalizedCoord::new(1.0))].into(), 20),
            ]))
            .unwrap();
        assert!(!regions.iter().any(|(r, _)| is_default(r)));
        let region_values: Vec<_> = regions.into_iter().map(|(_, v)| v + default).collect();
        assert_eq!((15, vec![10, 20]), (default, region_values));
    }
}
