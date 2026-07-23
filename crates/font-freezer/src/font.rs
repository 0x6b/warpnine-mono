//! Font parsing, freezing, and serialization.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::{Debug, Formatter},
    iter::once,
    result,
};

use read_fonts::{
    FontRef, TableProvider,
    tables::{cmap::CmapSubtable as ReadCmapSubtable, gsub::Gsub},
    types::{GlyphId16, NameId},
};
use write_fonts::{
    BuilderError, FontBuilder,
    tables::{
        cmap::{Cmap, Cmap4, CmapSubtable, EncodingRecord, PlatformId, SequentialMapGroup},
        name::{Name, NameRecord},
        post::Post,
    },
    types::Version16Dot16,
};

use crate::{Result, error::Error, gsub::GlyphSubstitutions, types::*};

/// A parsed font ready for feature freezing.
pub struct Font<'a> {
    data: &'a [u8],
    inner: FontRef<'a>,
}

impl Debug for Font<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font")
            .field("data_len", &self.data.len())
            .finish_non_exhaustive()
    }
}

impl<'a> TryFrom<&'a [u8]> for Font<'a> {
    type Error = Error;

    fn try_from(data: &'a [u8]) -> Result<Self> {
        Self::new(data)
    }
}

impl AsRef<[u8]> for Font<'_> {
    fn as_ref(&self) -> &[u8] {
        self.data
    }
}

impl<'a> Font<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data, inner: FontRef::new(data)? })
    }

    pub fn report(&self) -> Result<FontReport> {
        let gsub = self.inner.gsub().map_err(|_| Error::NoGsub)?;
        let script_list = gsub.script_list()?;

        let scripts_langs = script_list
            .script_records()
            .iter()
            .flat_map(|sr| {
                let tag = sr.script_tag();
                let langs = sr
                    .script(script_list.offset_data())
                    .into_iter()
                    .flat_map(|s| s.lang_sys_records())
                    .map(move |lr| format!("-s '{tag}' -l '{}'", lr.lang_sys_tag()));
                once(format!("-s '{tag}'")).chain(langs)
            })
            .collect();

        let features = gsub
            .feature_list()?
            .feature_records()
            .iter()
            .map(|r| r.feature_tag().to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        Ok(FontReport { scripts_langs, features })
    }

    pub fn freeze(&self, options: &FreezeOptions) -> Result<FreezeResult> {
        let gsub = self.inner.gsub().map_err(|_| Error::NoGsub)?;

        let lookup_indices = FeatureResolver { gsub: &gsub, options }.resolve()?;
        if lookup_indices.is_empty() {
            return Err(Error::NoMatchingFeatures(options.features.clone()));
        }

        let mut subs = GlyphSubstitutions::new();
        subs.process_lookups(&gsub, &lookup_indices)?;
        if subs.is_empty() {
            return Err(Error::NoSubstitutions(options.features.clone()));
        }

        // Only compute glyph info if we need warnings/names (expensive for large fonts)
        let (warnings, remapped_names) = if options.suffix.is_enabled() || options.warnings {
            GlyphInfo::from_font(&self.inner).analyze(&subs)
        } else {
            Default::default()
        };

        let mut data = FontEditor(self.inner.clone()).with_remapped_cmap(&subs)?;

        if options.wants_name_edits() {
            data = FontEditor::from_data(&data)?.with_modified_names(options)?;
        }
        if options.zapnames {
            data = FontEditor::from_data(&data)?.with_post_v3()?;
        }

        Ok(FreezeResult {
            data,
            stats: FreezeStats {
                features_requested: options.features.len(),
                lookups_processed: lookup_indices.len(),
                substitutions_applied: subs.len(),
            },
            warnings,
            remapped_names,
        })
    }

    pub fn data(&self) -> &[u8] {
        self.data
    }
}

struct FeatureResolver<'a> {
    gsub: &'a Gsub<'a>,
    options: &'a FreezeOptions,
}

impl FeatureResolver<'_> {
    // `.iter().map(|i| i.get())` over `&BigEndian<T>` reads as a redundant closure
    // to clippy, but the method-path form does not type-check: `get` takes `self`
    // by value while `iter()` yields `&BigEndian<T>`.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn resolve(&self) -> Result<BTreeSet<u16>> {
        let feature_indices = self.collect_feature_indices()?;
        let feature_tags = self.options.feature_tags();
        let feature_list = self.gsub.feature_list()?;

        Ok(feature_list
            .feature_records()
            .iter()
            .enumerate()
            .filter(|(i, _)| feature_indices.as_ref().is_none_or(|fi| fi.contains(&(*i as u16))))
            .filter(|(_, r)| feature_tags.contains(&r.feature_tag()))
            .flat_map(|(_, r)| {
                r.feature(feature_list.offset_data())
                    .into_iter()
                    .flat_map(|f| f.lookup_list_indices().iter().map(|i| i.get()))
            })
            .collect())
    }

    // See `resolve`: `.map(|i| i.get())` over `&BigEndian<T>` is a clippy
    // false-positive that cannot use the method-path form.
    #[allow(clippy::redundant_closure_for_method_calls)]
    fn collect_feature_indices(&self) -> Result<Option<HashSet<u16>>> {
        if !self.options.filter.is_active() {
            return Ok(None);
        }

        let script_list = self.gsub.script_list()?;
        let mut indices = HashSet::new();

        for sr in script_list.script_records() {
            if !self.options.filter.matches_script(&sr.script_tag().to_string()) {
                continue;
            }
            let Ok(script) = sr.script(script_list.offset_data()) else {
                continue;
            };

            if self.options.filter.lang.is_some() {
                for lr in script.lang_sys_records() {
                    if self.options.filter.matches_lang(&lr.lang_sys_tag().to_string())
                        && let Ok(ls) = lr.lang_sys(script.offset_data())
                    {
                        indices.extend(ls.feature_indices().iter().map(|i| i.get()));
                    }
                }
            } else if let Some(Ok(ls)) = script.default_lang_sys() {
                indices.extend(ls.feature_indices().iter().map(|i| i.get()));
            }
        }
        Ok(Some(indices))
    }
}

struct GlyphInfo {
    names: HashMap<u16, String>,
    has_unicode: HashSet<u16>,
}

impl GlyphInfo {
    fn from_font(font: &FontRef) -> Self {
        let names = font
            .post()
            .ok()
            .zip(font.maxp().ok())
            .map(|(post, maxp)| {
                (0..maxp.num_glyphs())
                    .filter_map(|gid| {
                        post.glyph_name(GlyphId16::new(gid)).map(|n| (gid, n.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let has_unicode = font
            .cmap()
            .ok()
            .map(|cmap| {
                cmap.encoding_records()
                    .iter()
                    .filter_map(|r| r.subtable(cmap.offset_data()).ok())
                    .flat_map(|st| st.iter().map(|(_, gid)| gid.to_u32() as u16))
                    .collect()
            })
            .unwrap_or_default();

        Self { names, has_unicode }
    }

    fn analyze(&self, subs: &GlyphSubstitutions) -> (Vec<String>, Vec<String>) {
        let (mut warnings, mut names) = (Vec::new(), Vec::new());
        for (&from, &to) in subs.iter().filter(|(f, t)| f != t) {
            let (from_uni, to_uni) =
                (self.has_unicode.contains(&from), self.has_unicode.contains(&to));
            if !from_uni && !to_uni {
                let (fn_, tn) = (
                    self.names.get(&from).map_or("?", String::as_str),
                    self.names.get(&to).map_or("?", String::as_str),
                );
                warnings.push(format!("Cannot remap '{fn_}' -> '{tn}' because neither has a Unicode value assigned in any of the cmap tables."));
            } else {
                names.push(self.names.get(&to).cloned().unwrap_or_else(|| format!("gid{to}")));
            }
        }
        (warnings, names)
    }
}

pub struct FontEditor<'a>(FontRef<'a>);

impl<'a> FontEditor<'a> {
    pub fn from_data(data: &'a [u8]) -> Result<Self> {
        Ok(Self(FontRef::new(data)?))
    }

    pub fn with_remapped_cmap(&self, subs: &GlyphSubstitutions) -> Result<Vec<u8>> {
        let cmap = self.0.cmap().map_err(|_| Error::NoCmap)?;

        let mut records: Vec<_> = cmap
            .encoding_records()
            .iter()
            .filter_map(|r| r.subtable(cmap.offset_data()).ok().map(|st| (r, st)))
            .map(|(record, subtable)| {
                // Preserve the original format (format 4 stays format 4, format 12 stays
                // format 12) and the original platform/encoding IDs. Previously this always
                // rewrote every subtable as format 12 under (0,4)/(3,10), which silently
                // dropped any (3,1) Windows-BMP subtable a merged font provided; Windows
                // requires a 'cmap' subtable matching the 'name' table's (3,1) records or
                // it refuses to load the font at all.
                let is_format4 = matches!(subtable, ReadCmapSubtable::Format4(_));

                // read_fonts' format 4 iterator surfaces the mandatory terminating segment
                // (start=end=0xFFFF, idDelta=1, idRangeOffset=0) as a mapping to glyph 0,
                // since it only special-cases glyph 0 for the explicit-array lookup path,
                // not the idDelta path. Glyph 0 is .notdef and was never really "mapped" to
                // U+FFFF; keeping this phantom entry produces an extra segment that
                // duplicates the terminator's char-code range, corrupting the rebuilt table.
                let mut mappings: Vec<_> = subtable
                    .iter()
                    .filter(|(_, gid)| gid.to_u32() != 0)
                    .map(|(cp, gid)| (cp, subs.remap(gid.to_u32() as u16)))
                    .collect();
                mappings.sort_by_key(|&(cp, _)| cp);

                let platform_id = PlatformId::new(record.platform_id() as u16);
                let encoding_id = record.encoding_id();

                let new_subtable = if is_format4 {
                    build_format4_subtable(&mappings)
                } else {
                    CmapSubtable::format_12(0, build_groups(&mappings))
                };

                EncodingRecord::new(platform_id, encoding_id, new_subtable)
            })
            .collect();

        // The spec requires encoding records to be sorted by (platform ID, encoding ID);
        // some Windows cmap parsers rely on this for lookup and treat an unsorted table
        // as malformed.
        records.sort();

        self.rebuild(|b| b.add_table(&Cmap::new(records)).map(|_| ()))
    }

    pub fn with_modified_names(&self, options: &FreezeOptions) -> Result<Vec<u8>> {
        let name = self.0.name()?;
        let records = name.name_record();
        let string_data = name.string_data();
        let family_old = records
            .iter()
            .find_map(|r| match r.name_id() {
                id if id == NameId::TYPOGRAPHIC_FAMILY_NAME || id == NameId::FAMILY_NAME => {
                    r.string(string_data).ok().map(|s| s.to_string())
                }
                _ => None,
            })
            .unwrap_or_else(|| "UnknownFamily".to_string());

        let mut family = family_old.clone();
        if let Some(ref replacements) = options.replacenames {
            for (from, to) in replacements.split(',').filter_map(|s| s.split_once('/')) {
                family = family.replace(from, to);
            }
        }
        let family_new = format!("{family}{}", options.suffix_string());
        let (family_old_ns, family_new_ns) =
            (family_old.replace(' ', ""), family_new.replace(' ', ""));
        let features_csv = options.features.join(",");

        let records: Vec<_> = records
            .iter()
            .map(|r| {
                let orig = r.string(string_data).map(|s| s.to_string()).unwrap_or_default();
                let new_string = match r.name_id().to_u16() {
                    1 | 4 | 16 | 18 | 21 => orig.replace(&family_old, &family_new),
                    3 => format!("{orig};featfreeze:{features_csv}"),
                    5 if options.info => format!("{orig}; featfreeze: {features_csv}"),
                    6 | 20 => orig.replace(&family_old_ns, &family_new_ns),
                    _ => orig,
                };
                NameRecord::new(
                    r.platform_id(),
                    r.encoding_id(),
                    r.language_id(),
                    NameId::new(r.name_id().to_u16()),
                    new_string.into(),
                )
            })
            .collect();

        self.rebuild(|b| b.add_table(&Name::new(records)).map(|_| ()))
    }

    pub fn with_post_v3(&self) -> Result<Vec<u8>> {
        let post = self.0.post()?;
        let mut new_post = Post::new(
            post.italic_angle(),
            post.underline_position(),
            post.underline_thickness(),
            post.is_fixed_pitch(),
            0,
            0,
            0,
            0,
        );
        new_post.version = Version16Dot16::VERSION_3_0;
        self.rebuild(|b| b.add_table(&new_post).map(|_| ()))
    }

    fn rebuild(
        &self,
        add: impl FnOnce(&mut FontBuilder) -> result::Result<(), BuilderError>,
    ) -> Result<Vec<u8>> {
        let mut builder = FontBuilder::new();
        for rec in self.0.table_directory.table_records() {
            if let Some(data) = self.0.table_data(rec.tag()) {
                builder.add_raw(rec.tag(), data);
            }
        }
        add(&mut builder)?;
        Ok(builder.build())
    }
}

fn build_groups(mappings: &[(u32, u16)]) -> Vec<SequentialMapGroup> {
    let mut groups: Vec<SequentialMapGroup> = Vec::with_capacity(mappings.len());
    for &(cp, gid) in mappings {
        if let Some(last) = groups.last_mut() {
            let expected_cp = last.end_char_code + 1;
            let expected_gid =
                last.start_glyph_id + (last.end_char_code + 1 - last.start_char_code);
            if cp == expected_cp && u32::from(gid) == expected_gid {
                last.end_char_code = cp;
                continue;
            }
        }
        groups.push(SequentialMapGroup {
            start_char_code: cp,
            end_char_code: cp,
            start_glyph_id: u32::from(gid),
        });
    }
    groups
}

/// Build a format 4 subtable from BMP (co)domain-preserving `(codepoint, glyph_id)` pairs.
///
/// Every segment uses the explicit glyph-id-array encoding rather than `idDelta`, so segment
/// boundaries only need to track contiguous codepoint runs (not contiguous glyph ids too).
/// This mirrors `warpnine-font-merger`'s cmap format-4 builder.
fn build_format4_subtable(mappings: &[(u32, u16)]) -> CmapSubtable {
    // U+FFFF is excluded even though it's <= 0xFFFF: it collides with the char-code range
    // of the mandatory terminating segment appended below, and (being a Unicode
    // noncharacter) is never legitimately mapped by a real font anyway.
    let bmp: Vec<(u16, u16)> =
        mappings.iter().filter(|(cp, _)| *cp != 0xFFFF).map(|&(cp, gid)| (cp as u16, gid)).collect();

    let mut segments: Vec<(usize, usize)> = Vec::new();
    if !bmp.is_empty() {
        let mut seg_start = 0;
        for i in 1..bmp.len() {
            if bmp[i].0 != bmp[i - 1].0 + 1 {
                segments.push((seg_start, i - 1));
                seg_start = i;
            }
        }
        segments.push((seg_start, bmp.len() - 1));
    }

    // Plus the mandatory terminating segment (0xFFFF, 0xFFFF, idDelta=1).
    let n_segments = segments.len() + 1;

    let mut start_code = Vec::with_capacity(n_segments);
    let mut end_code = Vec::with_capacity(n_segments);
    let mut id_delta = Vec::with_capacity(n_segments);
    let mut id_range_offsets = Vec::with_capacity(n_segments);
    let mut glyph_id_array = Vec::new();

    for (i, &(start_ix, end_ix)) in segments.iter().enumerate() {
        start_code.push(bmp[start_ix].0);
        end_code.push(bmp[end_ix].0);
        id_delta.push(0i16);

        let n_following_segments = n_segments - i;
        let id_range_offset = (n_following_segments + glyph_id_array.len()) * 2;
        id_range_offsets.push(id_range_offset as u16);

        glyph_id_array.extend(bmp[start_ix..=end_ix].iter().map(|(_, gid)| *gid));
    }

    start_code.push(0xFFFF);
    end_code.push(0xFFFF);
    id_delta.push(1);
    id_range_offsets.push(0);

    CmapSubtable::Format4(Cmap4::new(0, end_code, start_code, id_delta, id_range_offsets, glyph_id_array))
}
