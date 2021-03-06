/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use font::{Font, FontGroup};
use font::SpecifiedFontStyle;
use platform::font_context::FontContextHandle;
use style::computed_values::{font_style, font_variant};

use font_cache_task::FontCacheTask;
use font_template::FontTemplateDescriptor;
use platform::font_template::FontTemplateData;
use font::FontHandleMethods;
use platform::font::FontHandle;
use servo_util::cache::HashCache;
use servo_util::smallvec::{SmallVec, SmallVec8};
use servo_util::geometry::Au;
use servo_util::arc_ptr_eq;

use std::rc::Rc;
use std::cell::RefCell;
use sync::Arc;

use azure::AzFloat;
use azure::azure_hl::SkiaBackend;
use azure::scaled_font::ScaledFont;

#[cfg(target_os="linux")]
#[cfg(target_os="android")]
use azure::scaled_font::FontData;

#[cfg(target_os="linux")]
#[cfg(target_os="android")]
fn create_scaled_font(template: &Arc<FontTemplateData>, pt_size: Au) -> ScaledFont {
    ScaledFont::new(SkiaBackend, FontData(&template.bytes), pt_size.to_subpx() as AzFloat)
}

#[cfg(target_os="macos")]
fn create_scaled_font(template: &Arc<FontTemplateData>, pt_size: Au) -> ScaledFont {
    let cgfont = template.ctfont.as_ref().unwrap().copy_to_CGFont();
    ScaledFont::new(SkiaBackend, &cgfont, pt_size.to_subpx() as AzFloat)
}

static SMALL_CAPS_SCALE_FACTOR: f64 = 0.8;      // Matches FireFox (see gfxFont.h)

struct LayoutFontCacheEntry {
    family: String,
    font: Option<Rc<RefCell<Font>>>,
}

struct FallbackFontCacheEntry {
    font: Rc<RefCell<Font>>,
}

/// A cached azure font (per render task) that
/// can be shared by multiple text runs.
struct RenderFontCacheEntry {
    pt_size: Au,
    identifier: String,
    font: Rc<RefCell<ScaledFont>>,
}

/// The FontContext represents the per-thread/task state necessary for
/// working with fonts. It is the public API used by the layout and
/// render code. It talks directly to the font cache task where
/// required.
pub struct FontContext {
    platform_handle: FontContextHandle,
    font_cache_task: FontCacheTask,

    /// TODO: See bug https://github.com/servo/servo/issues/3300.
    layout_font_cache: Vec<LayoutFontCacheEntry>,
    fallback_font_cache: Vec<FallbackFontCacheEntry>,

    /// Strong reference as the render FontContext is (for now) recycled
    /// per frame. TODO: Make this weak when incremental redraw is done.
    render_font_cache: Vec<RenderFontCacheEntry>,

    last_style: Option<Arc<SpecifiedFontStyle>>,
    last_fontgroup: Option<Rc<FontGroup>>,
}

impl FontContext {
    pub fn new(font_cache_task: FontCacheTask) -> FontContext {
        let handle = FontContextHandle::new();
        FontContext {
            platform_handle: handle,
            font_cache_task: font_cache_task,
            layout_font_cache: vec!(),
            fallback_font_cache: vec!(),
            render_font_cache: vec!(),
            last_style: None,
            last_fontgroup: None,
        }
    }

    /// Create a font for use in layout calculations.
    fn create_layout_font(&self, template: Arc<FontTemplateData>,
                            descriptor: FontTemplateDescriptor, pt_size: Au,
                            variant: font_variant::T) -> Font {
        // TODO: (Bug #3463): Currently we only support fake small-caps
        // rendering. We should also support true small-caps (where the
        // font supports it) in the future.
        let actual_pt_size = match variant {
            font_variant::small_caps => pt_size.scale_by(SMALL_CAPS_SCALE_FACTOR),
            font_variant::normal => pt_size,
        };

        let handle: FontHandle = FontHandleMethods::new_from_template(&self.platform_handle,
                                    template, Some(actual_pt_size)).unwrap();
        let metrics = handle.get_metrics();

        Font {
            handle: handle,
            shaper: None,
            variant: variant,
            descriptor: descriptor,
            requested_pt_size: pt_size,
            actual_pt_size: actual_pt_size,
            metrics: metrics,
            shape_cache: HashCache::new(),
            glyph_advance_cache: HashCache::new(),
        }
    }

    /// Create a group of fonts for use in layout calculations. May return
    /// a cached font if this font instance has already been used by
    /// this context.
    pub fn get_layout_font_group_for_style(&mut self, style: Arc<SpecifiedFontStyle>)
                                            -> Rc<FontGroup> {
        let matches = match self.last_style {
            Some(ref last_style) => arc_ptr_eq(&style, last_style),
            None => false,
        };
        if matches {
            return self.last_fontgroup.as_ref().unwrap().clone();
        }

        // TODO: The font context holds a strong ref to the cached fonts
        // so they will never be released. Find out a good time to drop them.

        let desc = FontTemplateDescriptor::new(style.font_weight,
                                               style.font_style == font_style::italic);
        let mut fonts = SmallVec8::new();

        for family in style.font_family.iter() {
            // GWTODO: Check on real pages if this is faster as Vec() or HashMap().
            let mut cache_hit = false;
            for cached_font_entry in self.layout_font_cache.iter() {
                if cached_font_entry.family.as_slice() == family.name() {
                    match cached_font_entry.font {
                        None => {
                            cache_hit = true;
                            break;
                        }
                        Some(ref cached_font_ref) => {
                            let cached_font = cached_font_ref.borrow();
                            if cached_font.descriptor == desc &&
                               cached_font.requested_pt_size == style.font_size &&
                               cached_font.variant == style.font_variant {
                                fonts.push((*cached_font_ref).clone());
                                cache_hit = true;
                                break;
                            }
                        }
                    }
                }
            }

            if !cache_hit {
                let font_template = self.font_cache_task.get_font_template(family.name()
                                                                                 .to_string(),
                                                                           desc.clone());
                match font_template {
                    Some(font_template) => {
                        let layout_font = self.create_layout_font(font_template,
                                                                  desc.clone(),
                                                                  style.font_size,
                                                                  style.font_variant);
                        let layout_font = Rc::new(RefCell::new(layout_font));
                        self.layout_font_cache.push(LayoutFontCacheEntry {
                            family: family.name().to_string(),
                            font: Some(layout_font.clone()),
                        });
                        fonts.push(layout_font);
                    }
                    None => {
                        self.layout_font_cache.push(LayoutFontCacheEntry {
                            family: family.name().to_string(),
                            font: None,
                        });
                    }
                }
            }
        }

        // If unable to create any of the specified fonts, create one from the
        // list of last resort fonts for this platform.
        if fonts.len() == 0 {
            let mut cache_hit = false;
            for cached_font_entry in self.fallback_font_cache.iter() {
                let cached_font = cached_font_entry.font.borrow();
                if cached_font.descriptor == desc &&
                            cached_font.requested_pt_size == style.font_size &&
                            cached_font.variant == style.font_variant {
                    fonts.push(cached_font_entry.font.clone());
                    cache_hit = true;
                    break;
                }
            }

            if !cache_hit {
                let font_template = self.font_cache_task.get_last_resort_font_template(desc.clone());
                let layout_font = self.create_layout_font(font_template,
                                                          desc.clone(),
                                                          style.font_size,
                                                          style.font_variant);
                let layout_font = Rc::new(RefCell::new(layout_font));
                self.fallback_font_cache.push(FallbackFontCacheEntry {
                    font: layout_font.clone(),
                });
                fonts.push(layout_font);
            }
        }

        let font_group = Rc::new(FontGroup::new(fonts));
        self.last_style = Some(style);
        self.last_fontgroup = Some(font_group.clone());
        font_group
    }

    /// Create a render font for use with azure. May return a cached
    /// reference if already used by this font context.
    pub fn get_render_font_from_template(&mut self,
                                         template: &Arc<FontTemplateData>,
                                         pt_size: Au)
                                         -> Rc<RefCell<ScaledFont>> {
        for cached_font in self.render_font_cache.iter() {
            if cached_font.pt_size == pt_size &&
               cached_font.identifier == template.identifier {
                return cached_font.font.clone();
            }
        }

        let render_font = Rc::new(RefCell::new(create_scaled_font(template, pt_size)));
        self.render_font_cache.push(RenderFontCacheEntry{
            font: render_font.clone(),
            pt_size: pt_size,
            identifier: template.identifier.clone(),
        });
        render_font
    }

    /// Returns a reference to the font cache task.
    pub fn font_cache_task(&self) -> FontCacheTask {
        self.font_cache_task.clone()
    }
}
