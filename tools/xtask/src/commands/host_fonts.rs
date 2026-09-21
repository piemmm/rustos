//! A build-time font provider over the committed store, so the icon
//! verification the image build runs sees the same glyphs the desktop will.
//!
//! An icon asset carrying `<text>` is refused by the SVG decoder unless a
//! provider can furnish its faces, and the build refuses an icon the desktop
//! could not draw. Verifying such an asset therefore needs the same
//! resolution the running system does — so this drives the **real** `fontd`
//! service over the committed `lib/font/assets/` tree through the store
//! seam that service already has for host testing. Nothing about family
//! resolution, per-scalar fallback, weight instancing or outline decoding is
//! re-implemented here; only the on-disk reading is.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use tairix_abi::font_ipc::{
    decode_outlines_reply, FamilyKey, FontRequest, FontStretch, FontStyle, FontWeight, GlyphRun,
    FONT_MAX_OUTLINE_REPLY,
};
use tairix_abi::Errno;
use tairix_font::{seam_outlines, OutlineReply};
use tairix_fontd::discovery::{discover, FaceLoad, FontStore};
use tairix_fontd::FontService;
use tairix_log::{Event, Sink};
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_svg::font::{
    FaceId, FaceMetrics, FaceRequest, FontProvider, FontStyle as SvgStyle, FontUnavailable,
    GlyphOutline,
};

/// The RAM figure the build-time glyph cache is sized from: a build host has
/// memory to spare and the cache lives for one verification pass.
const HOST_RAM_BYTES: u64 = 1 << 30;

/// The hash key the build-time cache files its entries under. A build is not
/// serving hostile clients, so a fixed key keeps a verification pass
/// reproducible.
const HOST_HASH_KEY: tairix_hash::HashSeed =
    tairix_hash::HashSeed::from_words(0x7873_6B00_464F_4E54, 0x7873_6B00_464F_4E55);

/// A log sink for a pass that has nowhere to send records.
struct DiscardSink;

impl Sink for DiscardSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// The committed font store as the service's own discovery reads it.
struct AssetStore {
    root: PathBuf,
    /// Face bytes read so far, leaked into the arena the service borrows
    /// from for the life of the pass.
    read: BTreeMap<PathBuf, &'static [u8]>,
}

impl FontStore<'static> for AssetStore {
    fn family_dirs(&mut self) -> Result<Vec<String>, Errno> {
        let mut dirs = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|_| Errno::NotFound)? {
            let entry = entry.map_err(|_| Errno::NotFound)?;
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    dirs.push(String::from(name));
                }
            }
        }
        Ok(dirs)
    }

    fn read_manifest(&mut self, dir: &str) -> Option<String> {
        fs::read_to_string(self.root.join(dir).join("FontFamily")).ok()
    }

    fn face_loader(&mut self, dir: &str, face: &str) -> Box<dyn FaceLoad<'static> + 'static> {
        let path = self.root.join(dir).join(face);
        let bytes = match self.read.get(&path) {
            Some(held) => Some(*held),
            None => fs::read(&path).ok().map(|owned| {
                // Leaked deliberately: the service borrows a face for its
                // whole life, and a verification pass *is* that life. The
                // process exits when the build step does.
                let leaked: &'static [u8] = Box::leak(owned.into_boxed_slice());
                self.read.insert(path.clone(), leaked);
                leaked
            }),
        };
        Box::new(HostFace(bytes))
    }
}

/// One face's bytes, already read.
struct HostFace(Option<&'static [u8]>);

impl FaceLoad<'static> for HostFace {
    fn load(&mut self) -> Result<&'static [u8], Errno> {
        self.0.ok_or(Errno::NotFound)
    }
}

/// The build's font provider: the real service, over the committed store.
pub struct HostFonts {
    service: FontService<'static>,
    faces: Vec<(FamilyKey, FontWeight, FontStyle, FontStretch)>,
    reply: Vec<u8>,
}

impl HostFonts {
    /// Build a provider over the font assets under `assets_root` (the
    /// `lib/font/assets/` tree the image build plants as `/System/Fonts`).
    ///
    /// # Errors
    ///
    /// A message when the store cannot be listed or holds no usable family,
    /// which is the same fatal startup condition the running service has.
    pub fn open(assets_root: &Path) -> Result<Self, String> {
        static SINK: DiscardSink = DiscardSink;
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        let cache = tairix_fontd::glyph_cache(HOST_RAM_BYTES, gauge, &SINK, HOST_HASH_KEY);
        let mut store = AssetStore {
            root: assets_root.to_path_buf(),
            read: BTreeMap::new(),
        };
        let service = discover(&mut store, cache, &SINK).map_err(|err| {
            format!(
                "font store at {} is unusable: {err:?}",
                assets_root.display()
            )
        })?;
        Ok(Self {
            service,
            faces: Vec::new(),
            reply: vec![0; FONT_MAX_OUTLINE_REPLY],
        })
    }

    /// Ask the service for one run's outlines, exactly as a client would.
    fn ask(
        &mut self,
        family: FamilyKey,
        weight: FontWeight,
        style: FontStyle,
        stretch: FontStretch,
        scalars: &[char],
    ) -> Option<OutlineReply> {
        let run = GlyphRun::new(scalars).ok()?;
        let request = FontRequest::Outlines {
            family,
            scalars: run,
            weight,
            style,
            stretch,
        }
        .to_le_bytes();
        let len = self.service.handle(&request, &mut self.reply);
        let frame = self.reply.get(..len)?;
        let batch = decode_outlines_reply(frame).ok()?;
        Some(OutlineReply::from_batch(&batch))
    }
}

impl FontProvider for HostFonts {
    fn select(&mut self, req: &FaceRequest<'_>) -> Result<FaceMetrics, FontUnavailable> {
        let family = FamilyKey::new(req.family).map_err(|_| FontUnavailable)?;
        let weight = FontWeight::new(req.weight).map_err(|_| FontUnavailable)?;
        let style = match req.style {
            SvgStyle::Normal => FontStyle::Normal,
            SvgStyle::Italic => FontStyle::Italic,
            SvgStyle::Oblique => FontStyle::Oblique,
        };
        let stretch = FontStretch::new(req.stretch).map_err(|_| FontUnavailable)?;
        let instance = (family, weight, style, stretch);
        let held = self.faces.iter().position(|face| *face == instance);
        let reply = self
            .ask(family, weight, style, stretch, &[' '])
            .ok_or(FontUnavailable)?;
        let index = if let Some(index) = held {
            index
        } else {
            self.faces.push(instance);
            self.faces.len() - 1
        };
        let metrics = FaceMetrics {
            id: FaceId::new(u32::try_from(index).map_err(|_| FontUnavailable)?),
            units_per_em: f64::from(reply.units_per_em),
            ascent: f64::from(reply.ascent),
            descent: f64::from(reply.descent),
            line_gap: f64::from(reply.line_gap),
        };
        if metrics.is_usable() {
            Ok(metrics)
        } else {
            Err(FontUnavailable)
        }
    }

    fn outlines(
        &mut self,
        face: FaceId,
        run: &[char],
        out: &mut Vec<GlyphOutline>,
    ) -> Result<(), FontUnavailable> {
        let (family, weight, style, stretch) =
            *self.faces.get(face.get() as usize).ok_or(FontUnavailable)?;
        let mut left = run;
        while !left.is_empty() {
            let reply = self
                .ask(family, weight, style, stretch, left)
                .ok_or(FontUnavailable)?;
            if reply.glyphs.is_empty() {
                return Err(FontUnavailable);
            }
            let answered = reply.glyphs.len();
            out.extend(seam_outlines(reply));
            left = &left[answered..];
        }
        Ok(())
    }
}
