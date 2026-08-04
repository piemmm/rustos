//! The merged font family: an ordered set of faces with earliest-wins
//! codepoint resolution.
//!
//! A family lists its faces in resolution order. A codepoint resolves to the
//! first face whose `cmap` maps it — the primary face owns Latin, and a
//! companion is reached only for what the primary does not map — so coverage
//! is layered by order alone, with nothing per-face to keep in sync.

use alloc::vec::Vec;

use crate::engine::Face;
use crate::{AxisSetting, CellGeometry, FontError};

/// An ordered family of faces.
///
/// [`resolve`](Self::resolve) picks the earliest face that maps a codepoint;
/// [`merged`](Self::merged) walks the whole merged repertoire in codepoint
/// order (the atlas generator's build order).
pub struct FontFamily<'a> {
    faces: Vec<Face<'a>>,
}

impl<'a> FontFamily<'a> {
    /// Parse each face's bytes into the family, in order, at each face's
    /// default instance.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] if any face fails to parse, or if no faces are
    /// given.
    pub fn parse(sources: &[&'a [u8]]) -> Result<Self, FontError> {
        Self::parse_instance(sources, &[])
    }

    /// Parse each face's bytes into the family, in order, instancing every
    /// variable face at the given axis `settings` (so a whole family renders
    /// at one weight). A static face ignores the settings and is unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] if any face fails to parse, or if no faces are
    /// given.
    pub fn parse_instance(
        sources: &[&'a [u8]],
        settings: &[AxisSetting],
    ) -> Result<Self, FontError> {
        if sources.is_empty() {
            return Err(FontError::new("font family has no faces"));
        }
        let faces = sources
            .iter()
            .map(|&data| Face::parse_instance(data, settings))
            .collect::<Result<Vec<_>, FontError>>()?;
        Ok(Self { faces })
    }

    /// The primary face — the first in the family.
    #[must_use]
    pub fn primary(&self) -> &Face<'a> {
        &self.faces[0]
    }

    /// The face at family index `index`, so a caller can read a resolved
    /// face's metrics, or `None` when the index is out of range.
    #[must_use]
    pub fn face(&self, index: usize) -> Option<&Face<'a>> {
        self.faces.get(index)
    }

    /// Resolve `code` to `(face index, glyph)`: the earliest face whose `cmap`
    /// maps it. `None` when no face covers it.
    #[must_use]
    pub fn resolve(&self, code: u32) -> Option<(usize, u16)> {
        self.faces
            .iter()
            .enumerate()
            .find_map(|(index, face)| face.glyph_for(code).map(|glyph| (index, glyph)))
    }

    /// Rasterise the glyph at family `face_index` into `bitmap_width ×
    /// geometry.height` bytes of 4-bit coverage. See
    /// [`Face::rasterise_glyph`].
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] on a bad `face_index` or a malformed outline.
    pub fn rasterise(
        &self,
        face_index: usize,
        glyph: u16,
        geometry: &CellGeometry,
        px_per_em: f64,
        bitmap_width: u32,
    ) -> Result<Vec<u8>, FontError> {
        let face = self
            .faces
            .get(face_index)
            .ok_or(FontError::new("face index out of range"))?;
        face.rasterise_glyph(glyph, geometry, px_per_em, bitmap_width)
    }

    /// The whole merged repertoire in ascending codepoint order, as
    /// `(codepoint, face index, glyph)` triples.
    ///
    /// For a codepoint mapped by more than one face, the earliest face wins
    /// and the others are skipped — the exact set and order the atlas
    /// generator emits its cells in.
    #[must_use]
    pub fn merged(&self) -> Vec<(u32, usize, u16)> {
        let mut all: Vec<(u32, usize, u16)> = self
            .faces
            .iter()
            .enumerate()
            .flat_map(|(index, face)| {
                face.mapped()
                    .iter()
                    .map(move |&(code, glyph)| (code, index, glyph))
            })
            .collect();
        // Ordered by codepoint, then by face index, so the earliest face's
        // entry is the first of each codepoint's run and the rest drop out.
        all.sort_unstable_by_key(|&(code, index, _)| (code, index));
        all.dedup_by_key(|&mut (code, _, _)| code);
        all
    }
}
