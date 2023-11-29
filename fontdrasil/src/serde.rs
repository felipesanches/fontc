use crate::coords::{CoordConverter, DesignCoord, Location, UserCoord};
use serde::{Deserialize, Serialize};
use write_fonts::types::Tag;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct CoordConverterSerdeRepr {
    default_idx: usize,
    user_to_design: Vec<(f32, f32)>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct LocationSerdeRepr<T>(Vec<(Tag, T)>);

impl From<CoordConverterSerdeRepr> for CoordConverter {
    fn from(from: CoordConverterSerdeRepr) -> Self {
        let examples = from
            .user_to_design
            .into_iter()
            .map(|(u, d)| (UserCoord::new(u), DesignCoord::new(d)))
            .collect();
        CoordConverter::new(examples, from.default_idx)
    }
}

impl From<CoordConverter> for CoordConverterSerdeRepr {
    fn from(from: CoordConverter) -> Self {
        let user_to_design = from
            .user_to_design
            .from
            .iter()
            .zip(from.user_to_design.to)
            .map(|(u, d)| (u.into_inner(), d.into_inner()))
            .collect();
        CoordConverterSerdeRepr {
            default_idx: from.default_idx,
            user_to_design,
        }
    }
}

impl<T: Clone> From<LocationSerdeRepr<T>> for Location<T> {
    fn from(src: LocationSerdeRepr<T>) -> Location<T> {
        Location(src.0.into_iter().collect())
    }
}

impl<T: Clone> From<Location<T>> for LocationSerdeRepr<T> {
    fn from(src: Location<T>) -> LocationSerdeRepr<T> {
        LocationSerdeRepr(src.0.into_iter().collect())
    }
}
