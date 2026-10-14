//! The debugger's font for signal names and values, from Preferences.
use eframe::egui::{self, FontData, FontDefinitions, FontFamily, FontId};
use std::path::Path;

use crate::storage::{DebuggerFont, SignalFont};

/// The family a chosen font file is registered under. It is always bound,
/// to the ordinary proportional fonts when there is no file, so a frame that
/// names it before new fonts take effect still has something to draw with.
const FAMILY: &str = "debugger-signal";

/// No font file is anywhere near this; the cap keeps a wrong choice from
/// being read into memory whole.
const MAX_FONT_FILE: u64 = 64 * 1024 * 1024;

/// How signal names and values are drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalFonts {
    pub name: FontId,
    pub value: FontId,
}

impl Default for SignalFonts {
    fn default() -> Self {
        Self::new(&DebuggerFont::default(), false)
    }
}

impl SignalFonts {
    /// `file_loaded` says whether a chosen font file was registered; without
    /// it the default look is used.
    pub fn new(preference: &DebuggerFont, file_loaded: bool) -> Self {
        let size = f32::from(preference.size);
        let (name, value) = match &preference.font {
            SignalFont::Proportional => (FontFamily::Proportional, FontFamily::Proportional),
            SignalFont::Monospace => (FontFamily::Monospace, FontFamily::Monospace),
            SignalFont::File(_) if file_loaded => {
                let family = FontFamily::Name(FAMILY.into());
                (family.clone(), family)
            }
            SignalFont::Default | SignalFont::File(_) => {
                (FontFamily::Proportional, FontFamily::Monospace)
            }
        };
        Self {
            name: FontId::new(size, name),
            value: FontId::new(size, value),
        }
    }

    /// A row tall enough for the larger of the two, and never shorter than
    /// `minimum`.
    pub fn row_height(&self, minimum: f32) -> f32 {
        (self.name.size.max(self.value.size) * 1.5 + 6.0).max(minimum)
    }
}

/// A font file's bytes and family name, once it is known that egui can read
/// it. egui panics on a file it cannot parse, so nothing unchecked is given
/// to it.
pub fn read(path: &Path) -> Result<(Vec<u8>, String), String> {
    let shown = path.display();
    let length = std::fs::metadata(path)
        .map_err(|error| format!("Could not read {shown}: {error}"))?
        .len();
    if length > MAX_FONT_FILE {
        return Err(format!("{shown} is too large to be a font"));
    }
    let bytes = std::fs::read(path).map_err(|error| format!("Could not read {shown}: {error}"))?;
    let name = family_name(&bytes)
        .map_err(|error| format!("{shown} is not a font egui can use: {error}"))?;
    Ok((bytes, name))
}

/// The family name a font gives itself, after the same parse egui does
/// first. The file name is no guide: `consola.ttf` is Consolas.
fn family_name(bytes: &[u8]) -> Result<String, String> {
    use skrifa::{MetadataProvider as _, string::StringId};
    let font = skrifa::FontRef::from_index(bytes, 0).map_err(|error| error.to_string())?;
    // A damaged file can pass that check yet draw nothing, everything then
    // falling back to another font; signal names need letters and digits.
    let charmap = font.charmap();
    if !"Aa0_"
        .chars()
        .all(|character| charmap.map(character).is_some())
    {
        return Err("it has no letters or digits to draw signal names with".into());
    }
    Ok([StringId::TYPOGRAPHIC_FAMILY_NAME, StringId::FAMILY_NAME]
        .into_iter()
        .find_map(|id| font.localized_strings(id).english_or_first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Unnamed font".into()))
}

/// egui's own fonts, with the chosen file (if any) registered first in its
/// family and the proportional fonts behind it for any character it lacks.
pub fn definitions(file: Option<Vec<u8>>) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    let mut family = Vec::new();
    if let Some(bytes) = file {
        definitions
            .font_data
            .insert(FAMILY.into(), FontData::from_owned(bytes).into());
        family.push(FAMILY.to_owned());
    }
    family.extend(
        definitions.families[&FontFamily::Proportional]
            .iter()
            .cloned(),
    );
    definitions
        .families
        .insert(FontFamily::Name(FAMILY.into()), family);
    definitions
}

/// Register the preference's fonts with `ctx`, returning what to draw with
/// and, for a file that could not be used, why.
pub fn apply(ctx: &egui::Context, preference: &DebuggerFont) -> (SignalFonts, Option<String>) {
    let (file, error) = match &preference.font {
        SignalFont::File(path) => match read(path) {
            Ok((bytes, _)) => (Some(bytes), None),
            Err(error) => (None, Some(error)),
        },
        _ => (None, None),
    };
    let loaded = file.is_some();
    ctx.set_fonts(definitions(file));
    (SignalFonts::new(preference, loaded), error)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::test_support::TestDir;

    /// A real font to choose: one of egui's own, written to a file.
    pub(crate) fn font_file(dir: &TestDir) -> std::path::PathBuf {
        let definitions = FontDefinitions::default();
        let path = dir.path().join("Hack-Regular.ttf");
        std::fs::write(&path, &*definitions.font_data["Hack"].font).unwrap();
        path
    }

    #[test]
    fn a_font_file_is_checked_and_named_before_egui_is_given_it() {
        let dir = TestDir::new();
        let (bytes, name) = read(&font_file(&dir)).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(name, "Hack");

        let text = dir.path().join("notes.ttf");
        std::fs::write(&text, b"not a font").unwrap();
        assert!(read(&text).unwrap_err().contains("not a font egui can use"));
        assert!(
            read(&dir.path().join("missing.ttf"))
                .unwrap_err()
                .starts_with("Could not read")
        );
    }

    #[test]
    fn names_and_values_take_the_chosen_family_and_size() {
        let font = |font, size| DebuggerFont { font, size };
        assert_eq!(
            SignalFonts::new(&font(SignalFont::Default, 13), false),
            SignalFonts {
                name: FontId::proportional(13.0),
                value: FontId::monospace(13.0),
            }
        );
        let mono = SignalFonts::new(&font(SignalFont::Monospace, 20), false);
        assert_eq!(
            (mono.name, mono.value.clone()),
            (FontId::monospace(20.0), FontId::monospace(20.0))
        );
        let proportional = SignalFonts::new(&font(SignalFont::Proportional, 9), false);
        assert_eq!(proportional.value, FontId::proportional(9.0));
        let file = font(SignalFont::File("x.ttf".into()), 16);
        let named = FontId::new(16.0, FontFamily::Name(FAMILY.into()));
        assert_eq!(SignalFonts::new(&file, true).name, named);
        // A file that could not be loaded falls back to the default look.
        assert_eq!(
            SignalFonts::new(&file, false).value,
            FontId::monospace(16.0)
        );
        // Rows grow with the font but never shrink below their usual height.
        assert_eq!(
            SignalFonts::new(&font(SignalFont::Default, 8), false).row_height(24.0),
            24.0
        );
        assert_eq!(
            SignalFonts::new(&font(SignalFont::Default, 32), false).row_height(24.0),
            54.0
        );
    }

    #[test]
    fn a_chosen_font_is_drawn_and_a_bad_one_falls_back_without_panicking() {
        let dir = TestDir::new();
        let ctx = egui::Context::default();
        let draw = |fonts: &SignalFonts| {
            let mut output = ctx.run_ui(Default::default(), |ui| {
                ui.label(egui::RichText::new("Power_On").font(fonts.name.clone()));
            });
            output.textures_delta.clear();
            output
        };
        let chosen = DebuggerFont {
            font: SignalFont::File(font_file(&dir)),
            size: 18,
        };
        let (fonts, error) = apply(&ctx, &chosen);
        assert_eq!(error, None);
        assert_eq!(fonts.name.family, FontFamily::Name(FAMILY.into()));
        // The new fonts take effect from the next pass.
        draw(&fonts);
        draw(&fonts);

        let broken = dir.path().join("broken.otf");
        std::fs::write(&broken, b"OTTO and then nothing").unwrap();
        let (fonts, error) = apply(
            &ctx,
            &DebuggerFont {
                font: SignalFont::File(broken),
                size: 18,
            },
        );
        assert!(error.is_some());
        assert_eq!(
            fonts,
            SignalFonts::new(
                &DebuggerFont {
                    font: SignalFont::Default,
                    size: 18
                },
                false
            )
        );
        draw(&fonts);
        // The family stays bound, so a frame still naming it draws.
        draw(&SignalFonts::new(&chosen, true));
    }
}
