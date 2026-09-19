//! The splat field's weight vector, and the one operation that changes it.
//!
//! The world generator hands each cell a normalised blend of materials.
//! Three things then want to change that blend before it is drawn — a road
//! or river wearing in, snow settling, a scorch mark from a spell — and if
//! each did it its own way there would be three ways for a weight vector
//! to stop summing to what it must.
//!
//! So there is one mutation, [`WeightField::cover`], and everything that
//! changes the ground goes through it. It is the Porter–Duff *over*
//! operator on a weight vector: "this material now covers this fraction of
//! the cell, and everything already here shares what is left".
//!
//! # Why covering is a maximum and not a sum
//!
//! Two roads meeting must merge, not double. Summing gives a junction that
//! is somehow *more* road than either road, which either clips or bleeds
//! into the surrounding grass depending on where the clamp lands. Taking
//! the maximum makes a second stamp at the same coverage a no-op exactly,
//! so a junction is a road and the order the two were stamped in does not
//! show.
//!
//! # Why it is capped at four
//!
//! [`BLEND_SLOTS`] is what the splat pass reads in one go. A fifth
//! material at a cell is a boundary between boundaries that nothing would
//! resolve on screen, so a stamp lighter than everything already present
//! is refused rather than admitted by displacing something heavier — the
//! field stays the four materials that are actually visible.

use tairix_wintersun_world::biome::{Blend, Material, BLEND_SLOTS, WEIGHT_TOTAL};

/// What a field's weights sum to, always.
pub const TOTAL: u16 = WEIGHT_TOTAL;

/// One material's share of a cell.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Slot {
    /// The material.
    pub material: Material,
    /// Its share, out of [`TOTAL`].
    pub weight: u16,
}

/// A cell's materials and their shares, heaviest first.
///
/// The weights sum to [`TOTAL`] and no slot is empty, so a consumer never
/// has to check either. Slots are ordered by descending weight and, where
/// two weights are equal, by ascending material id — a total order, so two
/// fields holding the same materials hold them in the same places and fold
/// to the same digest.
#[derive(Copy, Clone, Debug)]
pub struct WeightField {
    slots: [Slot; BLEND_SLOTS],
    used: usize,
}

/// Equality is over the slots in use.
///
/// Deriving it would compare the unused tail, so two fields agreeing on
/// every material a consumer can see would differ on padding no consumer
/// can.
impl PartialEq for WeightField {
    fn eq(&self, other: &Self) -> bool {
        self.slots() == other.slots()
    }
}

impl Eq for WeightField {}

impl WeightField {
    /// A field of one material.
    #[must_use]
    pub fn solid(material: Material) -> Self {
        Self {
            slots: [Slot {
                material,
                weight: TOTAL,
            }; BLEND_SLOTS],
            used: 1,
        }
    }

    /// The field a generated cell's blend describes.
    ///
    /// A blend's weights already sum to [`TOTAL`]; empty slots are dropped
    /// so the splat does not read a texture for a material contributing
    /// nothing.
    #[must_use]
    pub fn from_blend(blend: &Blend) -> Self {
        let mut entries = [(blend.dominant(), 0u32); BLEND_SLOTS];
        let mut count = 0;
        for (&material, &weight) in blend.materials().iter().zip(blend.weights().iter()) {
            if weight == 0 {
                continue;
            }
            entries[count] = (material, u32::from(weight));
            count += 1;
        }
        if count == 0 {
            // A generated blend is normalised, so this is unreachable from
            // the generator; falling back to the label rather than to an
            // empty field keeps the invariant true for any input at all.
            return Self::solid(blend.dominant());
        }
        Self::from_entries(&mut entries[..count])
    }

    /// The materials and their shares, heaviest first.
    #[must_use]
    pub fn slots(&self) -> &[Slot] {
        &self.slots[..self.used]
    }

    /// The heaviest material.
    #[must_use]
    pub fn dominant(&self) -> Material {
        self.slots[0].material
    }

    /// The weights' sum, which is always [`TOTAL`].
    #[must_use]
    pub fn total(&self) -> u16 {
        self.slots().iter().map(|s| s.weight).sum()
    }

    /// This material's share, or zero if it is not present.
    #[must_use]
    pub fn weight_of(&self, material: Material) -> u16 {
        self.slots()
            .iter()
            .find(|s| s.material == material)
            .map_or(0, |s| s.weight)
    }

    /// Lay `material` over the field so that it covers at least
    /// `coverage`/[`TOTAL`] of the cell, sharing what is left among
    /// everything already there.
    ///
    /// Returns whether the field changed. A coverage at or below the
    /// material's current share is a no-op, which is what makes two
    /// overlapping stamps merge; a stamp lighter than every material
    /// already present on a full field is refused, and says so.
    pub fn cover(&mut self, material: Material, coverage: u16) -> bool {
        let target = coverage.min(TOTAL);
        let held = self.weight_of(material);
        if target <= held {
            return false;
        }
        let present = held > 0;
        if !present && self.used == BLEND_SLOTS && target <= self.slots[self.used - 1].weight {
            return false;
        }

        // `held < target <= TOTAL`, so the room the other slots share is
        // non-zero and the rescale below cannot divide by nothing.
        let remaining = u32::from(TOTAL - target);
        let others = u32::from(TOTAL - held);
        let mut entries = [(material, 0u32); BLEND_SLOTS + 1];
        let mut count = 0;
        for slot in self.slots() {
            if slot.material == material {
                continue;
            }
            entries[count] = (slot.material, u32::from(slot.weight) * remaining / others);
            count += 1;
        }
        entries[count] = (material, u32::from(target));
        count += 1;
        *self = Self::from_entries(&mut entries[..count]);
        true
    }

    /// The field `t`/255 of the way from `self` to `other`.
    ///
    /// Materials present in only one of the two fade in or out rather than
    /// appearing at a boundary, which is what lets a caller interpolate a
    /// cell grid into a pixel grid without the grid showing.
    #[must_use]
    pub fn lerp(&self, other: &Self, t: u8) -> Self {
        let near = u32::from(u8::MAX - t);
        let far = u32::from(t);
        let mut entries = [(self.dominant(), 0u32); BLEND_SLOTS * 2];
        let mut count = 0;
        for (slots, share) in [(self.slots(), near), (other.slots(), far)] {
            for slot in slots {
                let weighted = u32::from(slot.weight) * share;
                if let Some(entry) = entries[..count]
                    .iter_mut()
                    .find(|(held, _)| *held == slot.material)
                {
                    entry.1 += weighted;
                } else {
                    entries[count] = (slot.material, weighted);
                    count += 1;
                }
            }
        }
        Self::from_entries(&mut entries[..count])
    }

    /// Build a normalised field from unnormalised `(material, weight)`
    /// entries, keeping the heaviest [`BLEND_SLOTS`].
    ///
    /// The entries are sorted in place by descending weight and ascending
    /// material id, which is the total order the canonical form needs; the
    /// rounding remainder goes to the heaviest slot, so the sum is exactly
    /// [`TOTAL`] however the division fell.
    ///
    /// `entries` must be non-empty and hold at least one non-zero weight.
    fn from_entries(entries: &mut [(Material, u32)]) -> Self {
        entries.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id().cmp(&b.0.id())));
        let kept = entries.len().min(BLEND_SLOTS);
        let sum: u32 = entries[..kept].iter().map(|e| e.1).sum();
        let label = entries.first().map_or(Material::Water, |e| e.0);
        if sum == 0 {
            return Self::solid(label);
        }

        let mut slots = [Slot {
            material: label,
            weight: 0,
        }; BLEND_SLOTS];
        let mut used = 0;
        let mut assigned = 0u32;
        for entry in &entries[..kept] {
            let weight = entry.1 * u32::from(TOTAL) / sum;
            if weight == 0 {
                continue;
            }
            slots[used] = Slot {
                material: entry.0,
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a share of TOTAL is below it, and TOTAL is a u16"
                )]
                weight: weight as u16,
            };
            assigned += weight;
            used += 1;
        }
        // The heaviest entry holds at least `sum / BLEND_SLOTS`, so its
        // share is at least `TOTAL / BLEND_SLOTS` and at least one slot is
        // always kept.
        debug_assert!(used > 0, "every share floored to zero");
        #[allow(
            clippy::cast_possible_truncation,
            reason = "flooring every share leaves a remainder below the slot count"
        )]
        {
            slots[0].weight += (u32::from(TOTAL) - assigned) as u16;
        }
        Self {
            slots,
            used: used.max(1),
        }
    }
}

#[cfg(test)]
mod tests;
