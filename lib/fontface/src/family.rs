//! The merged font family: an ordered set of scoped faces with earliest-wins
//! codepoint resolution.

use alloc::vec::Vec;

use crate::engine::Face;
use crate::FontError;

/// The repertoire a face contributes to the merged family.
///
/// A face's `cmap` may map far more than the family wants from it (a Korean
/// coding face also carries Latin), so each face is scoped: a codepoint the
/// scope excludes is left to a later face.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Repertoire {
    /// Every mapped codepoint the face supplies (that no earlier face already
    /// supplied).
    Full,
    /// Korean letters only: compatibility jamo and precomposed syllables.
    Korean,
}

impl Repertoire {
    /// Whether `code` is inside this repertoire.
    #[must_use]
    pub fn contains(self, code: u32) -> bool {
        match self {
            Self::Full => true,
            Self::Korean => (0x3130..=0x318F).contains(&code) || (0xAC00..=0xD7A3).contains(&code),
        }
    }
}

/// An ordered family of scoped faces.
///
/// [`resolve`](Self::resolve) picks the earliest face that both scopes and
/// maps a codepoint; [`merged`](Self::merged) walks the whole merged
/// repertoire in codepoint order (the atlas generator's build order).
pub struct FontFamily<'a> {
    faces: Vec<(Face<'a>, Repertoire)>,
}

impl<'a> FontFamily<'a> {
    /// Parse each `(data, repertoire)` source into a scoped face, in order.
    ///
    /// # Errors
    ///
    /// Returns a [`FontError`] if any face fails to parse, or if no faces are
    /// given.
    pub fn parse(sources: &[(&'a [u8], Repertoire)]) -> Result<Self, FontError> {
        if sources.is_empty() {
            return Err(FontError::new("font family has no faces"));
        }
        let faces = sources
            .iter()
            .map(|&(data, repertoire)| Ok((Face::parse(data)?, repertoire)))
            .collect::<Result<Vec<_>, FontError>>()?;
        Ok(Self { faces })
    }

    /// The primary face — the first in the family.
    #[must_use]
    pub fn primary(&self) -> &Face<'a> {
        &self.faces[0].0
    }

    /// Resolve `code` to `(face index, glyph)`: the earliest face whose
    /// repertoire scopes `code` and whose `cmap` maps it. `None` when no face
    /// covers it.
    #[must_use]
    pub fn resolve(&self, code: u32) -> Option<(usize, u16)> {
        self.faces
            .iter()
            .enumerate()
            .find_map(|(index, (face, repertoire))| {
                if repertoire.contains(code) {
                    face.glyph_for(code).map(|glyph| (index, glyph))
                } else {
                    None
                }
            })
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
        geometry: &crate::CellGeometry,
        px_per_em: f64,
        bitmap_width: u32,
    ) -> Result<Vec<u8>, FontError> {
        let (face, _) = self
            .faces
            .get(face_index)
            .ok_or(FontError::new("face index out of range"))?;
        face.rasterise_glyph(glyph, geometry, px_per_em, bitmap_width)
    }

    /// The whole merged repertoire in ascending codepoint order, as
    /// `(codepoint, face index, glyph)` triples.
    ///
    /// For a codepoint mapped by more than one scoping face, the earliest face
    /// wins and the others are skipped — the exact set and order the atlas
    /// generator emits its cells in.
    #[must_use]
    pub fn merged(&self) -> Vec<(u32, usize, u16)> {
        // Each face's scoped, sorted (code, glyph) view, plus a cursor.
        let scoped: Vec<Vec<(u32, u16)>> = self
            .faces
            .iter()
            .map(|(face, repertoire)| {
                face.mapped()
                    .iter()
                    .copied()
                    .filter(|&(code, _)| repertoire.contains(code))
                    .collect()
            })
            .collect();
        let mut cursors = alloc::vec![0usize; scoped.len()];
        let mut merged = Vec::new();
        while let Some((face_index, code, glyph)) = scoped
            .iter()
            .enumerate()
            .filter_map(|(face_index, entries)| {
                entries
                    .get(cursors[face_index])
                    .map(|&(code, glyph)| (face_index, code, glyph))
            })
            .min_by_key(|&(face_index, code, _)| (code, face_index))
        {
            for (entries, cursor) in scoped.iter().zip(cursors.iter_mut()) {
                if entries
                    .get(*cursor)
                    .is_some_and(|&(other_code, _)| other_code == code)
                {
                    *cursor += 1;
                }
            }
            merged.push((code, face_index, glyph));
        }
        merged
    }
}
