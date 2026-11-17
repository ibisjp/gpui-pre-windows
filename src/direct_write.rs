// Modified by ibis inc. for ibisOffice (2026-10-27): report DirectWrite font simulations for PDF/print.
// Modified by ibis inc. for ibisOffice (2026-10-01): implemented `PlatformTextSystem::font_data`.
// Modified by ibis inc. for ibisOffice (2026-10-12): `layout_line` keeps the lines it laid out, by text, size and font runs.
// Modified by ibis inc. for ibisOffice (2026-10-28): `PlatformTextSystem::font_variations` (the axis values of a variable font's face).
// Modified by ibis inc. for ibisOffice (2026-11-03): a color glyph with a COLR version 1 paint tree (gradients) is drawn by Direct2D; `glyph_for_char` is `None` for a character the font lacks.
// Modified by ibis inc. for ibisOffice (2026-10-25): a face DirectWrite emboldens (a family without a bold face) keeps the regular face's advances; `PlatformTextSystem::synthetic_style`.
use std::{
    borrow::Cow,
    ffi::{c_uint, c_void},
    mem::ManuallyDrop,
    sync::Arc,
};

use anyhow::{Context, Result};
use collections::HashMap;
use gpui_util::{ResultExt, maybe};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use windows::{
    Win32::{
        Foundation::*,
        Globalization::GetUserDefaultLocaleName,
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT},
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1DeviceContext7,
                ID2D1Factory1,
            },
            Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP, Direct3D11::*, DirectWrite::*,
            Dxgi::Common::*, Gdi::LOGFONTW,
            Imaging::{
                CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
                WICBitmapCacheOnLoad,
            },
        },
        System::{
            Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
            SystemServices::LOCALE_NAME_MAX_LENGTH,
        },
        UI::WindowsAndMessaging::*,
    },
    core::*,
};
use windows_numerics::{Matrix3x2, Vector2};

use crate::*;
use gpui::*;

#[derive(Debug)]
struct FontInfo {
    font_family_h: HSTRING,
    /// The width of the face: normal, but for a face asked for by its Win32 family name (Arial Narrow), which text formats ask
    /// for as its typographic family (Arial) at that width.
    font_stretch: DWRITE_FONT_STRETCH,
    font_face: IDWriteFontFace3,
    features: IDWriteTypography,
    fallbacks: Option<IDWriteFontFallback>,
    font_collection: IDWriteFontCollection1,
}

pub(crate) struct DirectWriteTextSystem {
    components: DirectWriteComponents,
    state: RwLock<DirectWriteState>,
}

struct DirectWriteComponents {
    locale: HSTRING,
    factory: IDWriteFactory5,
    in_memory_loader: IDWriteInMemoryFontFileLoader,
    builder: IDWriteFontSetBuilder1,
    text_renderer: TextRendererWrapper,
    system_ui_font_name: SharedString,
    system_subpixel_rendering: bool,
}

impl Drop for DirectWriteComponents {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .factory
                .UnregisterFontFileLoader(&self.in_memory_loader);
        }
    }
}

struct GPUState {
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    sampler: Option<ID3D11SamplerState>,
    blend_state: ID3D11BlendState,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
}

struct DirectWriteState {
    gpu_state: GPUState,
    system_font_collection: IDWriteFontCollection1,
    custom_font_collection: IDWriteFontCollection1,
    fonts: Vec<FontInfo>,
    font_to_font_id: HashMap<Font, FontId>,
    font_info_cache: HashMap<usize, FontId>,
    layout_line_scratch: Vec<u16>,
    /// The lines laid out, by text, font size and font runs. A sheet printed
    /// to PDF lays out 430 000 cell texts of which 40 000 differ: the same
    /// text in the same font comes out the same, so it is laid out once.
    layout_cache: HashMap<LayoutKey, CachedLayout>,
    /// The data of font files read for `font_data`, by their loader and
    /// reference key (a file is read once: a collection may be tens of MB).
    font_files: HashMap<(usize, Vec<u8>), Arc<Vec<u8>>>,
}

/// What a laid out line depends on: its text, the font size (as bits), and
/// each run's font and length.
#[derive(Clone, PartialEq, Eq, Hash)]
struct LayoutKey {
    text: String,
    font_size: u32,
    runs: Vec<(usize, usize)>,
}

/// A laid out line (`LineLayout` is not `Clone`).
struct CachedLayout {
    width: Pixels,
    ascent: Pixels,
    descent: Pixels,
    runs: Vec<ShapedRun>,
    len: usize,
}

/// The lines kept at most; the cache starts again when it is full.
const LAYOUT_CACHE_LIMIT: usize = 200_000;

impl GPUState {
    fn new(directx_devices: &DirectXDevices) -> Result<Self> {
        let device = directx_devices.device.clone();
        let device_context = directx_devices.device_context.clone();

        let blend_state = {
            let mut blend_state = None;
            let desc = D3D11_BLEND_DESC {
                AlphaToCoverageEnable: false.into(),
                IndependentBlendEnable: false.into(),
                RenderTarget: [
                    D3D11_RENDER_TARGET_BLEND_DESC {
                        BlendEnable: true.into(),
                        SrcBlend: D3D11_BLEND_ONE,
                        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                        BlendOp: D3D11_BLEND_OP_ADD,
                        SrcBlendAlpha: D3D11_BLEND_ONE,
                        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                        BlendOpAlpha: D3D11_BLEND_OP_ADD,
                        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    },
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                ],
            };
            unsafe { device.CreateBlendState(&desc, Some(&mut blend_state)) }?;
            blend_state.unwrap()
        };

        let sampler = {
            let mut sampler = None;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_BORDER,
                AddressV: D3D11_TEXTURE_ADDRESS_BORDER,
                AddressW: D3D11_TEXTURE_ADDRESS_BORDER,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                BorderColor: [0.0, 0.0, 0.0, 0.0],
                MinLOD: 0.0,
                MaxLOD: 0.0,
            };
            unsafe { device.CreateSamplerState(&desc, Some(&mut sampler)) }?;
            sampler
        };

        let vertex_shader = {
            let source = shader_resources::RawShaderBytes::new(
                shader_resources::ShaderModule::EmojiRasterization,
                shader_resources::ShaderTarget::Vertex,
            )?;
            let mut shader = None;
            unsafe { device.CreateVertexShader(source.as_bytes(), None, Some(&mut shader)) }?;
            shader.unwrap()
        };

        let pixel_shader = {
            let source = shader_resources::RawShaderBytes::new(
                shader_resources::ShaderModule::EmojiRasterization,
                shader_resources::ShaderTarget::Fragment,
            )?;
            let mut shader = None;
            unsafe { device.CreatePixelShader(source.as_bytes(), None, Some(&mut shader)) }?;
            shader.unwrap()
        };

        Ok(Self {
            device,
            device_context,
            sampler,
            blend_state,
            vertex_shader,
            pixel_shader,
        })
    }
}

impl DirectWriteTextSystem {
    pub(crate) fn new(directx_devices: &DirectXDevices) -> Result<Self> {
        let factory: IDWriteFactory5 = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        // The `IDWriteInMemoryFontFileLoader` here is supported starting from
        // Windows 10 Creators Update, which consequently requires the entire
        // `DirectWriteTextSystem` to run on `win10 1703`+.
        let in_memory_loader = unsafe { factory.CreateInMemoryFontFileLoader()? };
        unsafe { factory.RegisterFontFileLoader(&in_memory_loader)? };
        let builder = unsafe { factory.CreateFontSetBuilder()? };
        let mut locale = [0u16; LOCALE_NAME_MAX_LENGTH as usize];
        unsafe { GetUserDefaultLocaleName(&mut locale) };
        let locale = HSTRING::from_wide(&locale);
        let text_renderer = TextRendererWrapper::new(locale.clone());

        let gpu_state = GPUState::new(directx_devices)?;

        let system_subpixel_rendering = get_system_subpixel_rendering();
        let system_ui_font_name = get_system_ui_font_name();
        let components = DirectWriteComponents {
            locale,
            factory,
            in_memory_loader,
            builder,
            text_renderer,
            system_ui_font_name,
            system_subpixel_rendering,
        };

        let system_font_collection = unsafe {
            let mut result = None;
            components
                .factory
                .GetSystemFontCollection(false, &mut result, true)?;
            result.context("Failed to get system font collection")?
        };
        let custom_font_set = unsafe { components.builder.CreateFontSet()? };
        let custom_font_collection = unsafe {
            components
                .factory
                .CreateFontCollectionFromFontSet(&custom_font_set)?
        };

        Ok(Self {
            components,
            state: RwLock::new(DirectWriteState {
                gpu_state,
                system_font_collection,
                custom_font_collection,
                fonts: Vec::new(),
                font_to_font_id: HashMap::default(),
                font_info_cache: HashMap::default(),
                layout_line_scratch: Vec::new(),
                layout_cache: HashMap::default(),
                font_files: HashMap::default(),
            }),
        })
    }

    pub(crate) fn handle_gpu_lost(&self, directx_devices: &DirectXDevices) -> Result<()> {
        self.state.write().handle_gpu_lost(directx_devices)
    }
}

impl PlatformTextSystem for DirectWriteTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.state.write().add_fonts(&self.components, fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.state.read().all_font_names(&self.components)
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let lock = self.state.upgradable_read();
        if let Some(font_id) = lock.font_to_font_id.get(font) {
            Ok(*font_id)
        } else {
            RwLockUpgradableReadGuard::upgrade(lock)
                .select_and_cache_font(&self.components, font)
                .with_context(|| format!("Failed to select font: {:?}", font))
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.state.read().font_metrics(font_id)
    }

    fn synthetic_style(&self, font_id: FontId) -> (bool, bool) {
        self.state.read().synthetic_style(font_id)
    }

    fn font_data(&self, font_id: FontId) -> Option<(Arc<Vec<u8>>, u32)> {
        self.state.write().font_data(font_id)
    }

    fn font_variations(&self, font_id: FontId) -> Vec<([u8; 4], f32)> {
        self.state.read().font_variations(font_id)
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        self.state.read().get_typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> anyhow::Result<Size<f32>> {
        self.state.read().get_advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.state.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(
        &self,
        params: &RenderGlyphParams,
    ) -> anyhow::Result<Bounds<DevicePixels>> {
        self.state.read().raster_bounds(&self.components, params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> anyhow::Result<(Size<DevicePixels>, Vec<u8>)> {
        self.state
            .read()
            .rasterize_glyph(&self.components, params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.state
            .write()
            .layout_line(&self.components, text, font_size, runs)
            .log_err()
            .unwrap_or(LineLayout {
                font_size,
                ..Default::default()
            })
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        if self.components.system_subpixel_rendering {
            TextRenderingMode::Subpixel
        } else {
            TextRenderingMode::Grayscale
        }
    }
}

impl DirectWriteState {
    fn select_and_cache_font(
        &mut self,
        components: &DirectWriteComponents,
        font: &Font,
    ) -> Option<FontId> {
        let select_font = |this: &mut DirectWriteState, font: &Font| -> Option<FontId> {
            let info = [&this.custom_font_collection, &this.system_font_collection]
                .into_iter()
                .find_map(|font_collection| unsafe {
                    DirectWriteState::make_font_from_font_collection(
                        font,
                        font_collection,
                        &components.factory,
                        &this.system_font_collection,
                        &components.system_ui_font_name,
                    )
                })?;

            let font_id = FontId(this.fonts.len());
            let font_face_key = info.font_face.cast::<IUnknown>().unwrap().as_raw().addr();
            this.fonts.push(info);
            this.font_info_cache.insert(font_face_key, font_id);
            Some(font_id)
        };

        let mut font_id = select_font(self, font);
        if font_id.is_none() {
            // try updating system fonts and reselect
            let mut collection = None;
            let font_collection_updated = unsafe {
                components
                    .factory
                    .GetSystemFontCollection(false, &mut collection, true)
            }
            .log_err()
            .is_some();
            if font_collection_updated && let Some(collection) = collection {
                self.system_font_collection = collection;
            }
            font_id = select_font(self, font);
        };
        let font_id = font_id?;
        self.font_to_font_id.insert(font.clone(), font_id);
        Some(font_id)
    }

    fn add_fonts(
        &mut self,
        components: &DirectWriteComponents,
        fonts: Vec<Cow<'static, [u8]>>,
    ) -> Result<()> {
        for font_data in fonts {
            match font_data {
                Cow::Borrowed(data) => unsafe {
                    let font_file = components
                        .in_memory_loader
                        .CreateInMemoryFontFileReference(
                            &components.factory,
                            data.as_ptr().cast(),
                            data.len() as _,
                            None,
                        )?;
                    components.builder.AddFontFile(&font_file)?;
                },
                Cow::Owned(data) => unsafe {
                    let font_file = components
                        .in_memory_loader
                        .CreateInMemoryFontFileReference(
                            &components.factory,
                            data.as_ptr().cast(),
                            data.len() as _,
                            None,
                        )?;
                    components.builder.AddFontFile(&font_file)?;
                },
            }
        }
        let set = unsafe { components.builder.CreateFontSet()? };
        let collection = unsafe { components.factory.CreateFontCollectionFromFontSet(&set)? };
        self.custom_font_collection = collection;

        Ok(())
    }

    fn generate_font_fallbacks(
        fallbacks: &FontFallbacks,
        factory: &IDWriteFactory5,
        system_font_collection: &IDWriteFontCollection1,
    ) -> Result<Option<IDWriteFontFallback>> {
        let fallback_list = fallbacks.fallback_list();
        if fallback_list.is_empty() {
            return Ok(None);
        }
        unsafe {
            let builder = factory.CreateFontFallbackBuilder()?;
            let font_set = &system_font_collection.GetFontSet()?;
            let mut unicode_ranges = Vec::new();
            for family_name in fallback_list {
                let family_name = HSTRING::from(family_name);
                let Some(fonts) = font_set
                    .GetMatchingFonts(
                        &family_name,
                        DWRITE_FONT_WEIGHT_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        DWRITE_FONT_STYLE_NORMAL,
                    )
                    .log_err()
                else {
                    continue;
                };
                let Ok(font_face) = fonts.GetFontFaceReference(0) else {
                    continue;
                };
                let font = font_face.CreateFontFace()?;
                let mut count = 0;
                font.GetUnicodeRanges(None, &mut count).ok();
                if count == 0 {
                    continue;
                }
                unicode_ranges.clear();
                unicode_ranges.resize_with(count as usize, DWRITE_UNICODE_RANGE::default);
                let Some(_) = font
                    .GetUnicodeRanges(Some(&mut unicode_ranges), &mut count)
                    .log_err()
                else {
                    continue;
                };
                builder.AddMapping(
                    &unicode_ranges,
                    &[family_name.as_ptr()],
                    None,
                    None,
                    None,
                    1.0,
                )?;
            }
            let system_fallbacks = factory.GetSystemFontFallback()?;
            builder.AddMappings(&system_fallbacks)?;
            Ok(Some(builder.CreateFontFallback()?))
        }
    }

    unsafe fn generate_font_features(
        factory: &IDWriteFactory5,
        font_features: &FontFeatures,
    ) -> Result<IDWriteTypography> {
        let direct_write_features = unsafe { factory.CreateTypography()? };
        apply_font_features(&direct_write_features, font_features)?;
        Ok(direct_write_features)
    }

    unsafe fn make_font_from_font_collection(
        &Font {
            ref family,
            ref features,
            ref fallbacks,
            weight,
            style,
        }: &Font,
        collection: &IDWriteFontCollection1,
        factory: &IDWriteFactory5,
        system_font_collection: &IDWriteFontCollection1,
        system_ui_font_name: &SharedString,
    ) -> Option<FontInfo> {
        const SYSTEM_UI_FONT_NAME: &str = ".SystemUIFont";
        let family = if family == SYSTEM_UI_FONT_NAME {
            system_ui_font_name
        } else {
            gpui::font_name_with_fallbacks_shared(&family, &system_ui_font_name)
        };
        let fontset = unsafe { collection.GetFontSet().log_err()? };
        let font_family_h = HSTRING::from(family.as_str());
        let font = unsafe {
            fontset
                .GetMatchingFonts(
                    &font_family_h,
                    font_weight_to_dwrite(weight),
                    DWRITE_FONT_STRETCH_NORMAL,
                    font_style_to_dwrite(style),
                )
                .log_err()?
        };
        let mut font = font;
        let mut total_number = unsafe { font.GetFontCount() };
        // A face of another width (Arial Narrow: width class 3) is in the family of its typographic name (Arial) as
        // a condensed face, and is not a family of its own; the name Office, GDI and the font's own name table give
        // it (the Win32 family name: Arial Narrow) finds it. The faces of that name are of one width: the weight
        // and the style choose among them.
        let mut by_win32 = false;
        if total_number == 0 {
            let value = HSTRING::from(family.as_str());
            let property = DWRITE_FONT_PROPERTY {
                propertyId: DWRITE_FONT_PROPERTY_ID_WIN32_FAMILY_NAME,
                propertyValue: PCWSTR(value.as_ptr()),
                localeName: w!("en-us"),
            };
            if let Some(found) = unsafe { fontset.GetMatchingFonts2(&[property]).log_err() } {
                font = found;
                total_number = unsafe { font.GetFontCount() };
                by_win32 = true;
            }
        }
        let mut order: Vec<u32> = (0..total_number).collect();
        if by_win32 {
            let distance = |index: u32| -> (bool, i32) {
                unsafe {
                    font.GetFontFaceReference(index)
                        .ok()
                        .and_then(|reference| reference.CreateFontFace().ok())
                        .map_or((true, i32::MAX), |face| {
                            (face.GetStyle() != font_style_to_dwrite(style), (face.GetWeight().0 - font_weight_to_dwrite(weight).0).abs())
                        })
                }
            };
            order.sort_by_key(|index| distance(*index));
        }
        for index in order {
            let res = maybe!({
                let font_face_ref = unsafe { font.GetFontFaceReference(index).log_err()? };
                let font_face = unsafe { font_face_ref.CreateFontFace().log_err()? };
                let direct_write_features =
                    unsafe { Self::generate_font_features(factory, features).log_err()? };
                let fallbacks = fallbacks.as_ref().and_then(|fallbacks| {
                    Self::generate_font_fallbacks(fallbacks, factory, system_font_collection)
                        .log_err()
                        .flatten()
                });
                let (font_family_h, font_stretch) = if by_win32 {
                    let typographic = unsafe { font_face.GetFamilyNames().log_err() }
                        .and_then(|names| get_name(names, &HSTRING::from("en-us")).log_err())
                        .map_or_else(|| font_family_h.clone(), |name| HSTRING::from(name));
                    (typographic, unsafe { font_face.GetStretch() })
                } else {
                    (font_family_h.clone(), DWRITE_FONT_STRETCH_NORMAL)
                };
                let font_info = FontInfo {
                    font_family_h,
                    font_stretch,
                    font_face,
                    features: direct_write_features,
                    fallbacks,
                    font_collection: collection.clone(),
                };
                Some(font_info)
            });
            if res.is_some() {
                return res;
            }
        }
        None
    }

    fn layout_line(
        &mut self,
        components: &DirectWriteComponents,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> Result<LineLayout> {
        if font_runs.is_empty() {
            return Ok(LineLayout {
                font_size,
                ..Default::default()
            });
        }
        let key = LayoutKey {
            text: text.to_string(),
            font_size: font_size.as_f32().to_bits(),
            runs: font_runs.iter().map(|run| (run.font_id.0, run.len)).collect(),
        };
        if let Some(cached) = self.layout_cache.get(&key) {
            return Ok(LineLayout {
                font_size,
                width: cached.width,
                ascent: cached.ascent,
                descent: cached.descent,
                runs: cached.runs.clone(),
                len: cached.len,
            });
        }
        unsafe {
            self.layout_line_scratch.clear();
            self.layout_line_scratch.extend(text.encode_utf16());
            let text_wide = &*self.layout_line_scratch;

            let mut utf8_offset = 0usize;
            let mut utf16_offset = 0u32;
            let text_layout = {
                let first_run = &font_runs[0];
                let font_info = &self.fonts[first_run.font_id.0];
                let collection = &font_info.font_collection;
                let format: IDWriteTextFormat1 = components
                    .factory
                    .CreateTextFormat(
                        &font_info.font_family_h,
                        collection,
                        font_info.font_face.GetWeight(),
                        font_info.font_face.GetStyle(),
                        font_info.font_stretch,
                        font_size.as_f32(),
                        &components.locale,
                    )?
                    .cast()?;
                if let Some(ref fallbacks) = font_info.fallbacks {
                    format.SetFontFallback(fallbacks)?;
                }

                let layout = components.factory.CreateTextLayout(
                    text_wide,
                    &format,
                    f32::INFINITY,
                    f32::INFINITY,
                )?;
                let current_text = &text[utf8_offset..(utf8_offset + first_run.len)];
                utf8_offset += first_run.len;
                let current_text_utf16_length = current_text.encode_utf16().count() as u32;
                let text_range = DWRITE_TEXT_RANGE {
                    startPosition: utf16_offset,
                    length: current_text_utf16_length,
                };
                layout.SetTypography(&font_info.features, text_range)?;
                utf16_offset += current_text_utf16_length;

                layout
            };

            let (ascent, descent) = {
                let mut first_metrics = [DWRITE_LINE_METRICS::default(); 4];
                let mut line_count = 0u32;
                text_layout.GetLineMetrics(Some(&mut first_metrics), &mut line_count)?;
                (
                    px(first_metrics[0].baseline),
                    px(first_metrics[0].height - first_metrics[0].baseline),
                )
            };
            let mut break_ligatures = true;
            for run in &font_runs[1..] {
                let font_info = &self.fonts[run.font_id.0];
                let current_text = &text[utf8_offset..(utf8_offset + run.len)];
                utf8_offset += run.len;
                let current_text_utf16_length = current_text.encode_utf16().count() as u32;

                let collection = &font_info.font_collection;
                let text_range = DWRITE_TEXT_RANGE {
                    startPosition: utf16_offset,
                    length: current_text_utf16_length,
                };
                utf16_offset += current_text_utf16_length;
                text_layout.SetFontCollection(collection, text_range)?;
                text_layout.SetFontFamilyName(&font_info.font_family_h, text_range)?;
                let font_size = if break_ligatures {
                    font_size.as_f32().next_up()
                } else {
                    font_size.as_f32()
                };
                text_layout.SetFontSize(font_size, text_range)?;
                text_layout.SetFontStyle(font_info.font_face.GetStyle(), text_range)?;
                text_layout.SetFontStretch(font_info.font_stretch, text_range)?;
                text_layout.SetFontWeight(font_info.font_face.GetWeight(), text_range)?;
                text_layout.SetTypography(&font_info.features, text_range)?;

                break_ligatures = !break_ligatures;
            }

            let mut runs = Vec::new();
            let mut renderer_context = RendererContext {
                text_system: self,
                components,
                index_converter: StringIndexConverter::new(text),
                runs: &mut runs,
                width: 0.0,
            };
            text_layout.Draw(
                Some((&raw mut renderer_context).cast::<c_void>().cast_const()),
                &components.text_renderer.0,
                0.0,
                0.0,
            )?;
            let width = px(renderer_context.width);

            if self.layout_cache.len() >= LAYOUT_CACHE_LIMIT {
                self.layout_cache.clear();
            }
            self.layout_cache.insert(
                key,
                CachedLayout {
                    width,
                    ascent,
                    descent,
                    runs: runs.clone(),
                    len: text.len(),
                },
            );
            Ok(LineLayout {
                font_size,
                width,
                ascent,
                descent,
                runs,
                len: text.len(),
            })
        }
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        unsafe {
            let font_info = &self.fonts[font_id.0];
            let mut metrics = std::mem::zeroed();
            font_info.font_face.GetMetrics(&mut metrics);

            FontMetrics {
                units_per_em: metrics.Base.designUnitsPerEm as _,
                ascent: metrics.Base.ascent as _,
                descent: -(metrics.Base.descent as f32),
                line_gap: metrics.Base.lineGap as _,
                underline_position: metrics.Base.underlinePosition as _,
                underline_thickness: metrics.Base.underlineThickness as _,
                cap_height: metrics.Base.capHeight as _,
                x_height: metrics.Base.xHeight as _,
                bounding_box: Bounds {
                    origin: Point {
                        x: metrics.glyphBoxLeft as _,
                        y: metrics.glyphBoxBottom as _,
                    },
                    size: Size {
                        width: (metrics.glyphBoxRight - metrics.glyphBoxLeft) as _,
                        height: (metrics.glyphBoxTop - metrics.glyphBoxBottom) as _,
                    },
                },
            }
        }
    }

    /// The data of the file the font comes from, read through its loader (so
    /// an installed font and one added from memory alike), and the font's
    /// index in it. A font made bold or italic by DirectWrite gives the file
    /// of the face it is made from.
    fn font_data(&mut self, font_id: FontId) -> Option<(Arc<Vec<u8>>, u32)> {
        let face = &self.fonts.get(font_id.0)?.font_face;
        unsafe {
            let index = face.GetIndex();
            let mut count = 1;
            let mut files = [None];
            face.GetFiles(&mut count, Some(files.as_mut_ptr())).log_err()?;
            let file = files[0].take()?;
            let mut key = std::ptr::null_mut();
            let mut key_size = 0;
            file.GetReferenceKey(&mut key, &mut key_size).log_err()?;
            let loader = file.GetLoader().log_err()?;
            let cache_key = (
                loader.as_raw().addr(),
                std::slice::from_raw_parts(key as *const u8, key_size as usize).to_vec(),
            );
            if let Some(data) = self.font_files.get(&cache_key) {
                return Some((data.clone(), index));
            }
            let stream = loader.CreateStreamFromKey(key, key_size).log_err()?;
            let size = stream.GetFileSize().log_err()?;
            let mut fragment = std::ptr::null_mut();
            let mut context = std::ptr::null_mut();
            stream
                .ReadFileFragment(&mut fragment, 0, size, &mut context)
                .log_err()?;
            let data = std::slice::from_raw_parts(fragment as *const u8, size as usize).to_vec();
            stream.ReleaseFileFragment(context);
            let data = Arc::new(data);
            self.font_files.insert(cache_key, data.clone());
            Some((data, index))
        }
    }

    fn create_glyph_run_analysis(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
    ) -> Result<IDWriteGlyphRunAnalysis> {
        let font = &self.fonts[params.font_id.0];
        let glyph_id = [params.glyph_id.0 as u16];
        let advance = [0.0];
        let offset = [DWRITE_GLYPH_OFFSET::default()];
        let glyph_run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(unsafe { std::ptr::read(&***font.font_face) })),
            fontEmSize: params.font_size.as_f32(),
            glyphCount: 1,
            glyphIndices: glyph_id.as_ptr(),
            glyphAdvances: advance.as_ptr(),
            glyphOffsets: offset.as_ptr(),
            isSideways: BOOL(0),
            bidiLevel: 0,
        };
        let transform = DWRITE_MATRIX {
            m11: params.scale_factor,
            m12: 0.0,
            m21: 0.0,
            m22: params.scale_factor,
            dx: 0.0,
            dy: 0.0,
        };
        let baseline_origin_x =
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor;
        let baseline_origin_y = params.subpixel_variant.y as f32
            / gpui::SUBPIXEL_VARIANTS_Y as f32
            / params.scale_factor;

        let mut rendering_mode = DWRITE_RENDERING_MODE1::default();
        let mut grid_fit_mode = DWRITE_GRID_FIT_MODE::default();
        unsafe {
            font.font_face.GetRecommendedRenderingMode(
                params.font_size.as_f32(),
                // Using 96 as scale is applied by the transform
                96.0,
                96.0,
                Some(&transform),
                false,
                DWRITE_OUTLINE_THRESHOLD_ANTIALIASED,
                DWRITE_MEASURING_MODE_NATURAL,
                None,
                &mut rendering_mode,
                &mut grid_fit_mode,
            )?;
        }
        let rendering_mode = match rendering_mode {
            DWRITE_RENDERING_MODE1_OUTLINE => DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
            m => m,
        };

        let antialias_mode = if params.subpixel_rendering {
            DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE
        } else {
            DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE
        };

        let glyph_analysis = unsafe {
            components.factory.CreateGlyphRunAnalysis(
                &glyph_run,
                Some(&transform),
                rendering_mode,
                DWRITE_MEASURING_MODE_NATURAL,
                grid_fit_mode,
                antialias_mode,
                baseline_origin_x,
                baseline_origin_y,
            )
        }?;
        Ok(glyph_analysis)
    }

    fn raster_bounds(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
    ) -> Result<Bounds<DevicePixels>> {
        let glyph_analysis = self.create_glyph_run_analysis(components, params)?;

        let texture_type = if params.subpixel_rendering {
            DWRITE_TEXTURE_CLEARTYPE_3x1
        } else {
            DWRITE_TEXTURE_ALIASED_1x1
        };

        let bounds = unsafe { glyph_analysis.GetAlphaTextureBounds(texture_type)? };

        if bounds.right < bounds.left {
            Ok(Bounds {
                origin: point(0.into(), 0.into()),
                size: size(0.into(), 0.into()),
            })
        } else {
            Ok(Bounds {
                origin: point(bounds.left.into(), bounds.top.into()),
                size: size(
                    (bounds.right - bounds.left).into(),
                    (bounds.bottom - bounds.top).into(),
                ),
            })
        }
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let font_info = &self.fonts[font_id.0];
        let codepoints = ch as u32;
        let mut glyph_indices = 0u16;
        unsafe {
            font_info
                .font_face
                .GetGlyphIndices(&raw const codepoints, 1, &raw mut glyph_indices)
                .log_err()
        }
        // Modified by ibis inc.: glyph 0 is `.notdef`, what a font without the character
        // maps it to; the character has no glyph (as the other platforms report it).
        .filter(|_| glyph_indices != 0)
        .map(|_| GlyphId(glyph_indices as u32))
    }

    fn rasterize_glyph(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let bitmap_data = if params.is_emoji {
            if let Ok(color) = self.rasterize_color(components, params, glyph_bounds) {
                color
            } else {
                let monochrome = self.rasterize_monochrome(components, params, glyph_bounds)?;
                monochrome
                    .into_iter()
                    .flat_map(|pixel| [0, 0, 0, pixel])
                    .collect::<Vec<_>>()
            }
        } else {
            self.rasterize_monochrome(components, params, glyph_bounds)?
        };

        Ok((glyph_bounds.size, bitmap_data))
    }

    fn rasterize_monochrome(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<Vec<u8>> {
        let glyph_analysis = self.create_glyph_run_analysis(components, params)?;
        if !params.subpixel_rendering {
            let mut bitmap_data =
                vec![0u8; glyph_bounds.size.width.0 as usize * glyph_bounds.size.height.0 as usize];
            unsafe {
                glyph_analysis.CreateAlphaTexture(
                    DWRITE_TEXTURE_ALIASED_1x1,
                    &RECT {
                        left: glyph_bounds.origin.x.0,
                        top: glyph_bounds.origin.y.0,
                        right: glyph_bounds.size.width.0 + glyph_bounds.origin.x.0,
                        bottom: glyph_bounds.size.height.0 + glyph_bounds.origin.y.0,
                    },
                    &mut bitmap_data,
                )?;
            }

            return Ok(bitmap_data);
        }

        let width = glyph_bounds.size.width.0 as usize;
        let height = glyph_bounds.size.height.0 as usize;
        let pixel_count = width * height;

        let mut bitmap_data = vec![0u8; pixel_count * 4];

        unsafe {
            glyph_analysis.CreateAlphaTexture(
                DWRITE_TEXTURE_CLEARTYPE_3x1,
                &RECT {
                    left: glyph_bounds.origin.x.0,
                    top: glyph_bounds.origin.y.0,
                    right: glyph_bounds.size.width.0 + glyph_bounds.origin.x.0,
                    bottom: glyph_bounds.size.height.0 + glyph_bounds.origin.y.0,
                },
                &mut bitmap_data[..pixel_count * 3],
            )?;
        }

        // The output buffer expects RGBA data, so pad the alpha channel with zeros.
        for pixel_ix in (0..pixel_count).rev() {
            let src = pixel_ix * 3;
            let dst = pixel_ix * 4;
            (
                bitmap_data[dst],
                bitmap_data[dst + 1],
                bitmap_data[dst + 2],
                bitmap_data[dst + 3],
            ) = (
                bitmap_data[src],
                bitmap_data[src + 1],
                bitmap_data[src + 2],
                0,
            );
        }

        Ok(bitmap_data)
    }

    fn rasterize_color(
        &self,
        components: &DirectWriteComponents,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<Vec<u8>> {
        // INVARIANT: the code below drives the *shared* D3D11 immediate context
        // (`Map`/`Unmap`/`Draw`/`CopyResource`), which `DirectXRenderer` and `DirectXAtlas` also
        // touch. An immediate `ID3D11DeviceContext` is not thread-safe, so this must only run on
        // the main UI thread (which it always is; text rasterization never leaves that thread).
        let bitmap_size = glyph_bounds.size;
        let subpixel_shift = params
            .subpixel_variant
            .map(|v| v as f32 / SUBPIXEL_VARIANTS_X as f32);
        let baseline_origin_x = subpixel_shift.x / params.scale_factor;
        let baseline_origin_y = subpixel_shift.y / params.scale_factor;

        let transform = DWRITE_MATRIX {
            m11: params.scale_factor,
            m12: 0.0,
            m21: 0.0,
            m22: params.scale_factor,
            dx: 0.0,
            dy: 0.0,
        };

        let font = &self.fonts[params.font_id.0];
        let glyph_id = [params.glyph_id.0 as u16];
        let advance = [glyph_bounds.size.width.0 as f32];
        let offset = [DWRITE_GLYPH_OFFSET {
            advanceOffset: -glyph_bounds.origin.x.0 as f32 / params.scale_factor,
            ascenderOffset: glyph_bounds.origin.y.0 as f32 / params.scale_factor,
        }];
        let glyph_run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(unsafe { std::ptr::read(&***font.font_face) })),
            fontEmSize: params.font_size.as_f32(),
            glyphCount: 1,
            glyphIndices: glyph_id.as_ptr(),
            glyphAdvances: advance.as_ptr(),
            glyphOffsets: offset.as_ptr(),
            isSideways: BOOL(0),
            bidiLevel: 0,
        };

        // Modified by ibis inc.: a glyph with a COLR version 1 paint tree (Segoe UI Emoji's
        // gradients on Windows 11) is drawn as the font draws it; its version 0 layers below
        // are flat colors the font keeps for older renderers.
        match Self::rasterize_paint_tree(
            components,
            &glyph_run,
            Vector2::new(baseline_origin_x, baseline_origin_y),
            params.scale_factor,
            bitmap_size,
        ) {
            Ok(Some(bitmap)) => return Ok(bitmap),
            Ok(None) => {}
            Err(error) => log::debug!("COLR v1 glyph not drawn by Direct2D: {error:?}"),
        }

        // todo: support formats other than COLR
        let color_enumerator = unsafe {
            components.factory.TranslateColorGlyphRun(
                Vector2::new(baseline_origin_x, baseline_origin_y),
                &glyph_run,
                None,
                DWRITE_GLYPH_IMAGE_FORMATS_COLR,
                DWRITE_MEASURING_MODE_NATURAL,
                Some(&transform),
                0,
            )
        }?;

        let mut glyph_layers = Vec::new();
        let mut alpha_data = Vec::new();
        loop {
            let color_run = unsafe { color_enumerator.GetCurrentRun() }?;
            let color_run = unsafe { &*color_run };
            let image_format = color_run.glyphImageFormat & !DWRITE_GLYPH_IMAGE_FORMATS_TRUETYPE;
            if image_format == DWRITE_GLYPH_IMAGE_FORMATS_COLR {
                let color_analysis = unsafe {
                    components.factory.CreateGlyphRunAnalysis(
                        &color_run.Base.glyphRun as *const _,
                        Some(&transform),
                        DWRITE_RENDERING_MODE1_NATURAL_SYMMETRIC,
                        DWRITE_MEASURING_MODE_NATURAL,
                        DWRITE_GRID_FIT_MODE_DEFAULT,
                        DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
                        baseline_origin_x,
                        baseline_origin_y,
                    )
                }?;

                let color_bounds =
                    unsafe { color_analysis.GetAlphaTextureBounds(DWRITE_TEXTURE_ALIASED_1x1) }?;

                let color_size = size(
                    color_bounds.right - color_bounds.left,
                    color_bounds.bottom - color_bounds.top,
                );
                if color_size.width > 0 && color_size.height > 0 {
                    alpha_data.clear();
                    alpha_data.resize((color_size.width * color_size.height) as usize, 0);
                    unsafe {
                        color_analysis.CreateAlphaTexture(
                            DWRITE_TEXTURE_ALIASED_1x1,
                            &color_bounds,
                            &mut alpha_data,
                        )
                    }?;

                    let run_color = {
                        let run_color = color_run.Base.runColor;
                        Rgba {
                            r: run_color.r,
                            g: run_color.g,
                            b: run_color.b,
                            a: run_color.a,
                        }
                    };
                    let bounds = bounds(point(color_bounds.left, color_bounds.top), color_size);
                    glyph_layers.push(GlyphLayerTexture::new(
                        &self.gpu_state,
                        run_color,
                        bounds,
                        &alpha_data,
                    )?);
                }
            }

            let has_next = unsafe { color_enumerator.MoveNext() }
                .map(|e| e.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
        }

        let gpu_state = &self.gpu_state;

        let render_target_texture = {
            let mut texture = None;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: bitmap_size.width.0 as u32,
                Height: bitmap_size.height.0 as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            unsafe {
                gpu_state
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }?;
            texture.unwrap()
        };

        let render_target_view = {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv = None;
            unsafe {
                gpu_state.device.CreateRenderTargetView(
                    &render_target_texture,
                    Some(&desc),
                    Some(&mut rtv),
                )
            }?;
            rtv
        };

        Self::composite_color_layers(
            gpu_state,
            &glyph_layers,
            bitmap_size,
            &render_target_texture,
            &render_target_view,
        )
    }

    /// Modified by ibis inc.: the glyph drawn from its COLR version 1 paint tree by Direct2D
    /// (`ID2D1DeviceContext7::DrawPaintGlyphRun`, Windows 11) into a bitmap of `bitmap_size`,
    /// straight-alpha BGRA as `rasterize_color` returns; `None` when the glyph has no paint
    /// tree. `transform`'s scale maps the run's units to the bitmap's pixels.
    fn rasterize_paint_tree(
        components: &DirectWriteComponents,
        glyph_run: &DWRITE_GLYPH_RUN,
        baseline_origin: Vector2,
        scale_factor: f32,
        bitmap_size: Size<DevicePixels>,
    ) -> Result<Option<Vec<u8>>> {
        let factory: IDWriteFactory8 = components.factory.cast()?;
        let Ok(enumerator) = (unsafe {
            factory.TranslateColorGlyphRun(
                baseline_origin,
                glyph_run,
                None,
                DWRITE_GLYPH_IMAGE_FORMATS_COLR_PAINT_TREE | DWRITE_GLYPH_IMAGE_FORMATS_TRUETYPE,
                DWRITE_PAINT_FEATURE_LEVEL_COLR_V1,
                DWRITE_MEASURING_MODE_NATURAL,
                None,
                0,
            )
        }) else {
            return Ok(None);
        };
        let mut runs = Vec::new();
        loop {
            let run = unsafe { &*enumerator.GetCurrentRun()? };
            runs.push((
                run.glyphImageFormat,
                Vector2::new(run.Base.baselineOriginX, run.Base.baselineOriginY),
                run.Base.glyphRun.clone(),
            ));
            if !unsafe { enumerator.MoveNext() }?.as_bool() {
                break;
            }
        }
        if !runs
            .iter()
            .any(|(format, ..)| *format == DWRITE_GLYPH_IMAGE_FORMATS_COLR_PAINT_TREE)
        {
            return Ok(None);
        }

        let (width, height) = (bitmap_size.width.0 as u32, bitmap_size.height.0 as u32);
        let wic: IWICImagingFactory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }?;
        let bitmap = unsafe {
            wic.CreateBitmap(width, height, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)
        }?;
        let d2d: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }?;
        let target = unsafe {
            d2d.CreateWicBitmapRenderTarget(
                &bitmap,
                &D2D1_RENDER_TARGET_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                },
            )
        }?;
        let context: ID2D1DeviceContext7 = target.cast()?;
        let black = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        let brush = unsafe { context.CreateSolidColorBrush(&black, None) }?;
        unsafe {
            context.BeginDraw();
            context.Clear(Some(&D2D1_COLOR_F::default()));
            context.SetTransform(&Matrix3x2 {
                M11: scale_factor,
                M12: 0.0,
                M21: 0.0,
                M22: scale_factor,
                M31: 0.0,
                M32: 0.0,
            });
            context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            for (format, origin, run) in &runs {
                if *format == DWRITE_GLYPH_IMAGE_FORMATS_COLR_PAINT_TREE {
                    context.DrawPaintGlyphRun(*origin, run, &brush, 0, DWRITE_MEASURING_MODE_NATURAL);
                } else {
                    context.DrawGlyphRun(*origin, run, None, &brush, DWRITE_MEASURING_MODE_NATURAL);
                }
            }
            context.EndDraw(None, None)?;
        }

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        unsafe { bitmap.CopyPixels(std::ptr::null(), width * 4, &mut pixels) }?;
        // Premultiplied to straight alpha, as the layers' composite is returned.
        for chunk in pixels.chunks_exact_mut(4) {
            let a = chunk[3] as f32;
            if a > 0.0 {
                for c in &mut chunk[..3] {
                    *c = (*c as f32 * 255.0 / a).clamp(0.0, 255.0) as u8;
                }
            }
        }
        Ok(Some(pixels))
    }

    fn composite_color_layers(
        gpu_state: &GPUState,
        glyph_layers: &[GlyphLayerTexture],
        bitmap_size: Size<DevicePixels>,
        render_target_texture: &ID3D11Texture2D,
        render_target_view: &Option<ID3D11RenderTargetView>,
    ) -> Result<Vec<u8>> {
        let params_buffer = {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<GlyphLayerTextureParams>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                MiscFlags: 0,
                StructureByteStride: 0,
            };

            let mut buffer = None;
            unsafe {
                gpu_state
                    .device
                    .CreateBuffer(&desc, None, Some(&mut buffer))
            }?;
            buffer
        };

        let staging_texture = {
            let mut texture = None;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: bitmap_size.width.0 as u32,
                Height: bitmap_size.height.0 as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            unsafe {
                gpu_state
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
            }?;
            texture.unwrap()
        };

        let device_context = &gpu_state.device_context;
        unsafe { device_context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP) };
        unsafe { device_context.VSSetShader(&gpu_state.vertex_shader, None) };
        unsafe { device_context.PSSetShader(&gpu_state.pixel_shader, None) };
        unsafe {
            device_context.VSSetConstantBuffers(0, Some(std::slice::from_ref(&params_buffer)))
        };
        unsafe {
            device_context.PSSetConstantBuffers(0, Some(std::slice::from_ref(&params_buffer)))
        };
        unsafe {
            device_context.OMSetRenderTargets(Some(std::slice::from_ref(render_target_view)), None)
        };
        unsafe {
            if let Some(render_target_view) = render_target_view.as_ref() {
                device_context.ClearRenderTargetView(render_target_view, &[0.0, 0.0, 0.0, 0.0]);
            }
        }
        unsafe { device_context.PSSetSamplers(0, Some(std::slice::from_ref(&gpu_state.sampler))) };
        unsafe { device_context.OMSetBlendState(&gpu_state.blend_state, None, 0xffffffff) };

        let crate::FontInfo {
            gamma_ratios,
            grayscale_enhanced_contrast,
            ..
        } = DirectXRenderer::get_font_info();

        for layer in glyph_layers {
            let params = GlyphLayerTextureParams {
                run_color: layer.run_color,
                bounds: layer.bounds,
                gamma_ratios: *gamma_ratios,
                grayscale_enhanced_contrast: *grayscale_enhanced_contrast,
                _pad: [0f32; 3],
            };
            unsafe {
                let mut dest = std::mem::zeroed();
                gpu_state.device_context.Map(
                    params_buffer.as_ref().unwrap(),
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut dest),
                )?;
                std::ptr::copy_nonoverlapping(&params as *const _, dest.pData as *mut _, 1);
                gpu_state
                    .device_context
                    .Unmap(params_buffer.as_ref().unwrap(), 0);
            };

            let texture = [Some(layer.texture_view.clone())];
            unsafe { device_context.PSSetShaderResources(0, Some(&texture)) };

            let viewport = [D3D11_VIEWPORT {
                TopLeftX: layer.bounds.origin.x as f32,
                TopLeftY: layer.bounds.origin.y as f32,
                Width: layer.bounds.size.width as f32,
                Height: layer.bounds.size.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }];
            unsafe { device_context.RSSetViewports(Some(&viewport)) };

            unsafe { device_context.Draw(4, 0) };
        }

        unsafe { device_context.CopyResource(&staging_texture, render_target_texture) };

        let mapped_data = {
            let mut mapped_data = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                device_context.Map(
                    &staging_texture,
                    0,
                    D3D11_MAP_READ,
                    0,
                    Some(&mut mapped_data),
                )
            }?;
            mapped_data
        };
        let mut rasterized =
            vec![0u8; (bitmap_size.width.0 as u32 * bitmap_size.height.0 as u32 * 4) as usize];

        for y in 0..bitmap_size.height.0 as usize {
            let width = bitmap_size.width.0 as usize;
            unsafe {
                std::ptr::copy_nonoverlapping::<u8>(
                    (mapped_data.pData as *const u8).byte_add(mapped_data.RowPitch as usize * y),
                    rasterized
                        .as_mut_ptr()
                        .byte_add(width * y * std::mem::size_of::<u32>()),
                    width * std::mem::size_of::<u32>(),
                )
            };
        }

        // Release the mapping now that the rows have been copied out; leaving `staging_texture`
        // mapped would leak the mapping and keep the resource pinned for later reuse.
        unsafe { device_context.Unmap(&staging_texture, 0) };

        // Convert from premultiplied to straight alpha
        for chunk in rasterized.chunks_exact_mut(4) {
            let b = chunk[0] as f32;
            let g = chunk[1] as f32;
            let r = chunk[2] as f32;
            let a = chunk[3] as f32;
            if a > 0.0 {
                let inv_a = 255.0 / a;
                chunk[0] = (b * inv_a).clamp(0.0, 255.0) as u8;
                chunk[1] = (g * inv_a).clamp(0.0, 255.0) as u8;
                chunk[2] = (r * inv_a).clamp(0.0, 255.0) as u8;
            }
        }

        Ok(rasterized)
    }

    fn get_typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        unsafe {
            let font = &self.fonts[font_id.0].font_face;
            let glyph_indices = [glyph_id.0 as u16];
            let mut metrics = [DWRITE_GLYPH_METRICS::default()];
            font.GetDesignGlyphMetrics(glyph_indices.as_ptr(), 1, metrics.as_mut_ptr(), false)?;

            let metrics = &metrics[0];
            let advance_width = metrics.advanceWidth as i32;
            let advance_height = metrics.advanceHeight as i32;
            let left_side_bearing = metrics.leftSideBearing;
            let right_side_bearing = metrics.rightSideBearing;
            let top_side_bearing = metrics.topSideBearing;
            let bottom_side_bearing = metrics.bottomSideBearing;
            let vertical_origin_y = metrics.verticalOriginY;

            let y_offset = vertical_origin_y + bottom_side_bearing - advance_height;
            let width = advance_width - (left_side_bearing + right_side_bearing);
            let height = advance_height - (top_side_bearing + bottom_side_bearing);

            Ok(Bounds {
                origin: Point {
                    x: left_side_bearing as f32,
                    y: y_offset as f32,
                },
                size: Size {
                    width: width as f32,
                    height: height as f32,
                },
            })
        }
    }

    /// Whether DirectWrite emboldens and slants the face (a family without a bold or italic face of its own): the glyphs
    /// the font file holds are the regular ones, so a PDF has to thicken and slant them itself.
    fn synthetic_style(&self, font_id: FontId) -> (bool, bool) {
        self.fonts.get(font_id.0).map_or((false, false), |info| {
            let simulations = unsafe { info.font_face.GetSimulations() }.0;
            (simulations & DWRITE_FONT_SIMULATIONS_BOLD.0 != 0, simulations & DWRITE_FONT_SIMULATIONS_OBLIQUE.0 != 0)
        })
    }

    /// The axis values (`wght`, `wdth`, ...) of the face when it is an instance of a variable font; empty otherwise.
    fn font_variations(&self, font_id: FontId) -> Vec<([u8; 4], f32)> {
        let Some(info) = self.fonts.get(font_id.0) else { return Vec::new() };
        let Ok(face5) = info.font_face.cast::<IDWriteFontFace5>() else { return Vec::new() };
        unsafe {
            if !face5.HasVariations().as_bool() {
                return Vec::new();
            }
            let count = face5.GetFontAxisValueCount() as usize;
            let mut values = vec![DWRITE_FONT_AXIS_VALUE::default(); count];
            if face5.GetFontAxisValues(&mut values).is_err() {
                return Vec::new();
            }
            values.iter().map(|v| (v.axisTag.0.to_le_bytes(), v.value)).collect()
        }
    }

    fn get_advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        unsafe {
            let font = &self.fonts[font_id.0].font_face;
            let glyph_indices = [glyph_id.0 as u16];
            let mut metrics = [DWRITE_GLYPH_METRICS::default()];
            font.GetDesignGlyphMetrics(glyph_indices.as_ptr(), 1, metrics.as_mut_ptr(), false)?;

            let metrics = &metrics[0];

            Ok(Size {
                width: metrics.advanceWidth as f32,
                height: 0.0,
            })
        }
    }

    fn all_font_names(&self, components: &DirectWriteComponents) -> Vec<String> {
        let mut result =
            get_font_names_from_collection(&self.system_font_collection, &components.locale);
        result.extend(get_font_names_from_collection(
            &self.custom_font_collection,
            &components.locale,
        ));
        result
    }

    fn handle_gpu_lost(&mut self, directx_devices: &DirectXDevices) -> Result<()> {
        try_to_recover_from_device_lost(|| {
            GPUState::new(directx_devices).context("Recreating GPU state for DirectWrite")
        })
        .map(|gpu_state| self.gpu_state = gpu_state)
    }
}

struct GlyphLayerTexture {
    run_color: Rgba,
    bounds: Bounds<i32>,
    texture_view: ID3D11ShaderResourceView,
    // holding on to the texture to not RAII drop it
    _texture: ID3D11Texture2D,
}

impl GlyphLayerTexture {
    fn new(
        gpu_state: &GPUState,
        run_color: Rgba,
        bounds: Bounds<i32>,
        alpha_data: &[u8],
    ) -> Result<Self> {
        let texture_size = bounds.size;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: texture_size.width as u32,
            Height: texture_size.height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let texture = {
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe {
                gpu_state
                    .device
                    .CreateTexture2D(&desc, None, Some(&mut texture))?
            };
            texture.unwrap()
        };
        let texture_view = {
            let mut view: Option<ID3D11ShaderResourceView> = None;
            unsafe {
                gpu_state
                    .device
                    .CreateShaderResourceView(&texture, None, Some(&mut view))?
            };
            view.unwrap()
        };

        unsafe {
            gpu_state.device_context.UpdateSubresource(
                &texture,
                0,
                None,
                alpha_data.as_ptr() as _,
                texture_size.width as u32,
                0,
            )
        };

        Ok(GlyphLayerTexture {
            run_color,
            bounds,
            texture_view,
            _texture: texture,
        })
    }
}

#[repr(C)]
struct GlyphLayerTextureParams {
    bounds: Bounds<i32>,
    run_color: Rgba,
    gamma_ratios: [f32; 4],
    grayscale_enhanced_contrast: f32,
    _pad: [f32; 3],
}

struct TextRendererWrapper(IDWriteTextRenderer);

impl TextRendererWrapper {
    fn new(locale_str: HSTRING) -> Self {
        let inner = TextRenderer::new(locale_str);
        TextRendererWrapper(inner.into())
    }
}

#[implement(IDWriteTextRenderer)]
struct TextRenderer {
    locale: HSTRING,
}

impl TextRenderer {
    fn new(locale: HSTRING) -> Self {
        TextRenderer { locale }
    }
}

struct RendererContext<'t, 'a, 'b> {
    text_system: &'t mut DirectWriteState,
    components: &'a DirectWriteComponents,
    index_converter: StringIndexConverter<'a>,
    runs: &'b mut Vec<ShapedRun>,
    width: f32,
}

#[derive(Debug)]
struct ClusterAnalyzer<'t> {
    utf16_idx: usize,
    glyph_idx: usize,
    glyph_count: usize,
    cluster_map: &'t [u16],
}

impl<'t> ClusterAnalyzer<'t> {
    fn new(cluster_map: &'t [u16], glyph_count: usize) -> Self {
        ClusterAnalyzer {
            utf16_idx: 0,
            glyph_idx: 0,
            glyph_count,
            cluster_map,
        }
    }
}

impl Iterator for ClusterAnalyzer<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        if self.utf16_idx >= self.cluster_map.len() {
            return None; // No more clusters
        }
        let start_utf16_idx = self.utf16_idx;
        let current_glyph = self.cluster_map[start_utf16_idx] as usize;

        // Find the end of current cluster (where glyph index changes)
        let mut end_utf16_idx = start_utf16_idx + 1;
        while end_utf16_idx < self.cluster_map.len()
            && self.cluster_map[end_utf16_idx] as usize == current_glyph
        {
            end_utf16_idx += 1;
        }

        let utf16_len = end_utf16_idx - start_utf16_idx;

        // Calculate glyph count for this cluster
        let next_glyph = if end_utf16_idx < self.cluster_map.len() {
            self.cluster_map[end_utf16_idx] as usize
        } else {
            self.glyph_count
        };

        let glyph_count = next_glyph - current_glyph;

        // Update state for next call
        self.utf16_idx = end_utf16_idx;
        self.glyph_idx = next_glyph;

        Some((utf16_len, glyph_count))
    }
}

#[allow(non_snake_case)]
impl IDWritePixelSnapping_Impl for TextRenderer_Impl {
    fn IsPixelSnappingDisabled(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
    ) -> windows::core::Result<BOOL> {
        Ok(BOOL(0))
    }

    fn GetCurrentTransform(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        transform: *mut DWRITE_MATRIX,
    ) -> windows::core::Result<()> {
        unsafe {
            *transform = DWRITE_MATRIX {
                m11: 1.0,
                m12: 0.0,
                m21: 0.0,
                m22: 1.0,
                dx: 0.0,
                dy: 0.0,
            };
        }
        Ok(())
    }

    fn GetPixelsPerDip(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
    ) -> windows::core::Result<f32> {
        Ok(1.0)
    }
}

#[allow(non_snake_case)]
impl IDWriteTextRenderer_Impl for TextRenderer_Impl {
    fn DrawGlyphRun(
        &self,
        clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _measuringmode: DWRITE_MEASURING_MODE,
        glyphrun: *const DWRITE_GLYPH_RUN,
        glyphrundescription: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        let glyphrun = unsafe { &*glyphrun };
        let glyph_count = glyphrun.glyphCount as usize;
        if glyph_count == 0 {
            return Ok(());
        }
        let desc = unsafe { &*glyphrundescription };
        let context = unsafe { &mut *(clientdrawingcontext.cast::<RendererContext>().cast_mut()) };
        let Some(font_face) = glyphrun.fontFace.as_ref() else {
            return Ok(());
        };
        // This `cast()` action here should never fail since we are running on Win10+, and
        // `IDWriteFontFace3` requires Win10
        let Ok(font_face) = &font_face.cast::<IDWriteFontFace3>() else {
            return Err(Error::new(
                DWRITE_E_UNSUPPORTEDOPERATION,
                "Failed to cast font face",
            ));
        };

        let font_face_key = font_face.cast::<IUnknown>().unwrap().as_raw().addr();
        let font_id = context
            .text_system
            .font_info_cache
            .get(&font_face_key)
            .copied()
            // in some circumstances, we might be getting served a FontFace that we did not create ourselves
            // so create a new font from it and cache it accordingly. The usual culprit here seems to be Segoe UI Symbol
            .map_or_else(
                || {
                    let font = font_face_to_font(font_face, &self.locale)
                        .ok_or_else(|| Error::new(DWRITE_E_NOFONT, "Failed to create font"))?;
                    let font_id = match context.text_system.font_to_font_id.get(&font) {
                        Some(&font_id) => font_id,
                        None => context
                            .text_system
                            .select_and_cache_font(context.components, &font)
                            .ok_or_else(|| Error::new(DWRITE_E_NOFONT, "Failed to create font"))?,
                    };
                    context
                        .text_system
                        .font_info_cache
                        .insert(font_face_key, font_id);
                    windows::core::Result::Ok(font_id)
                },
                Ok,
            )?;

        let color_font = unsafe { font_face.IsColorFont().as_bool() };

        let glyph_ids = unsafe {
            slice_from_nullable(
                glyphrun.glyphIndices,
                glyph_count,
                "DirectWrite returned a null glyph indices array",
            )?
        };
        let glyph_advances = unsafe {
            slice_from_nullable(
                glyphrun.glyphAdvances,
                glyph_count,
                "DirectWrite returned a null glyph advances array",
            )?
        };
        let glyph_offsets = unsafe {
            slice_from_nullable(
                glyphrun.glyphOffsets,
                glyph_count,
                "DirectWrite returned a null glyph offsets array",
            )?
        };
        let cluster_map = unsafe {
            slice_from_nullable(
                desc.clusterMap,
                desc.stringLength as usize,
                "DirectWrite returned a null cluster map",
            )?
        };

        let widening = simulated_bold_widening(font_face, glyph_ids, glyphrun.fontEmSize);
        let cluster_analyzer = ClusterAnalyzer::new(cluster_map, glyph_count);
        let mut utf16_idx = desc.textPosition as usize;
        let mut glyph_idx = 0;
        let mut glyphs = Vec::with_capacity(glyph_count);
        for (cluster_utf16_len, cluster_glyph_count) in cluster_analyzer {
            context.index_converter.advance_to_utf16_ix(utf16_idx);
            utf16_idx += cluster_utf16_len;
            for (cluster_glyph_idx, glyph_id) in glyph_ids
                [glyph_idx..(glyph_idx + cluster_glyph_count)]
                .iter()
                .enumerate()
            {
                let id = GlyphId(*glyph_id as u32);
                let is_emoji =
                    color_font && is_color_glyph(font_face, id, &context.components.factory);
                let this_glyph_idx = glyph_idx + cluster_glyph_idx;
                glyphs.push(ShapedGlyph {
                    id,
                    position: point(
                        px(context.width + glyph_offsets[this_glyph_idx].advanceOffset),
                        px(-glyph_offsets[this_glyph_idx].ascenderOffset),
                    ),
                    index: context.index_converter.utf8_ix,
                    is_emoji,
                });
                context.width += glyph_advances[this_glyph_idx] - widening.as_ref().map_or(0.0, |w| w[this_glyph_idx]);
            }
            glyph_idx += cluster_glyph_count;
        }
        context.runs.push(ShapedRun { font_id, glyphs });
        Ok(())
    }

    fn DrawUnderline(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _underline: *const DWRITE_UNDERLINE,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawUnderline unimplemented",
        ))
    }

    fn DrawStrikethrough(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _baselineoriginx: f32,
        _baselineoriginy: f32,
        _strikethrough: *const DWRITE_STRIKETHROUGH,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawStrikethrough unimplemented",
        ))
    }

    fn DrawInlineObject(
        &self,
        _clientdrawingcontext: *const ::core::ffi::c_void,
        _originx: f32,
        _originy: f32,
        _inlineobject: windows::core::Ref<IDWriteInlineObject>,
        _issideways: BOOL,
        _isrighttoleft: BOOL,
        _clientdrawingeffect: windows::core::Ref<windows::core::IUnknown>,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::new(
            E_NOTIMPL,
            "DrawInlineObject unimplemented",
        ))
    }
}

/// Interprets an optional DirectWrite array pointer as a slice, treating a
/// null pointer with a zero length as an empty slice. A null pointer with a
/// nonzero length fails with `null_error_message`.
///
/// # Safety
///
/// When `ptr` is non-null, the caller must guarantee that it points to a valid
/// array of at least `len` elements that outlives the returned slice.
unsafe fn slice_from_nullable<'a, T>(
    ptr: *const T,
    len: usize,
    null_error_message: &str,
) -> windows::core::Result<&'a [T]> {
    if ptr.is_null() {
        if len != 0 {
            return Err(Error::new(E_INVALIDARG, null_error_message));
        }
        Ok(&[])
    } else {
        Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

struct StringIndexConverter<'a> {
    text: &'a str,
    utf8_ix: usize,
    utf16_ix: usize,
}

impl<'a> StringIndexConverter<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            utf8_ix: 0,
            utf16_ix: 0,
        }
    }

    #[allow(dead_code)]
    fn advance_to_utf8_ix(&mut self, utf8_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf8_ix + ix >= utf8_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }

    fn advance_to_utf16_ix(&mut self, utf16_target: usize) {
        for (ix, c) in self.text[self.utf8_ix..].char_indices() {
            if self.utf16_ix >= utf16_target {
                self.utf8_ix += ix;
                return;
            }
            self.utf16_ix += c.len_utf16();
        }
        self.utf8_ix = self.text.len();
    }
}

fn font_style_to_dwrite(style: FontStyle) -> DWRITE_FONT_STYLE {
    match style {
        FontStyle::Normal => DWRITE_FONT_STYLE_NORMAL,
        FontStyle::Italic => DWRITE_FONT_STYLE_ITALIC,
        FontStyle::Oblique => DWRITE_FONT_STYLE_OBLIQUE,
    }
}

fn font_style_from_dwrite(value: DWRITE_FONT_STYLE) -> FontStyle {
    match value.0 {
        0 => FontStyle::Normal,
        1 => FontStyle::Italic,
        2 => FontStyle::Oblique,
        _ => unreachable!(),
    }
}

fn font_weight_to_dwrite(weight: FontWeight) -> DWRITE_FONT_WEIGHT {
    DWRITE_FONT_WEIGHT(weight.0 as i32)
}

fn font_weight_from_dwrite(value: DWRITE_FONT_WEIGHT) -> FontWeight {
    FontWeight(value.0 as f32)
}

fn get_font_names_from_collection(
    collection: &IDWriteFontCollection1,
    locale: &HSTRING,
) -> Vec<String> {
    unsafe {
        let mut result = Vec::new();
        let family_count = collection.GetFontFamilyCount();
        for index in 0..family_count {
            let Some(font_family) = collection.GetFontFamily(index).log_err() else {
                continue;
            };
            let Some(localized_family_name) = font_family.GetFamilyNames().log_err() else {
                continue;
            };
            let Some(family_name) = get_name(localized_family_name, locale).log_err() else {
                continue;
            };
            // The Win32 family names of the faces of another width than normal (Arial Narrow): Office lists them
            // as typefaces of their own, and a document names them so.
            for font_index in 0..font_family.GetFontCount() {
                let Some(font) = font_family.GetFont(font_index).log_err() else { continue };
                if font.GetStretch() == DWRITE_FONT_STRETCH_NORMAL {
                    continue;
                }
                let mut names = None;
                let mut exists = windows::core::BOOL(0);
                if font.GetInformationalStrings(DWRITE_INFORMATIONAL_STRING_WIN32_FAMILY_NAMES, &mut names, &mut exists).is_ok()
                    && exists.as_bool()
                    && let Some(names) = names
                    && let Some(win32) = get_name(names, locale).log_err()
                    && win32 != family_name
                    && !result.contains(&win32)
                {
                    result.push(win32);
                }
            }
            result.push(family_name);
        }

        result
    }
}

/// How much wider DirectWrite's simulated bold makes each glyph of a run than the face it emboldens (design units
/// scaled to the run's size). PowerPoint and ibisOffice's macOS text system draw such a bold by thickening the regular
/// glyphs without moving the next one, so the advance is the regular face's.
fn simulated_bold_widening(font_face: &IDWriteFontFace3, glyph_ids: &[u16], em_size: f32) -> Option<Vec<f32>> {
    unsafe {
        let simulations = font_face.GetSimulations();
        if simulations.0 & DWRITE_FONT_SIMULATIONS_BOLD.0 == 0 {
            return None;
        }
        let plain = font_face
            .GetFontFaceReference()
            .ok()?
            .CreateFontFaceWithSimulations(DWRITE_FONT_SIMULATIONS(simulations.0 & !DWRITE_FONT_SIMULATIONS_BOLD.0))
            .ok()?;
        let mut metrics = std::mem::zeroed();
        font_face.GetMetrics(&mut metrics);
        let scale = em_size / metrics.Base.designUnitsPerEm as f32;
        let mut emboldened = vec![DWRITE_GLYPH_METRICS::default(); glyph_ids.len()];
        let mut regular = vec![DWRITE_GLYPH_METRICS::default(); glyph_ids.len()];
        font_face.GetDesignGlyphMetrics(glyph_ids.as_ptr(), glyph_ids.len() as u32, emboldened.as_mut_ptr(), false).ok()?;
        plain.GetDesignGlyphMetrics(glyph_ids.as_ptr(), glyph_ids.len() as u32, regular.as_mut_ptr(), false).ok()?;
        Some(emboldened.iter().zip(&regular).map(|(e, r)| ((e.advanceWidth as f32 - r.advanceWidth as f32) * scale).max(0.0)).collect())
    }
}

fn font_face_to_font(font_face: &IDWriteFontFace3, locale: &HSTRING) -> Option<Font> {
    let localized_family_name = unsafe { font_face.GetFamilyNames().log_err() }?;
    let family_name = get_name(localized_family_name, locale).log_err()?;
    let weight = unsafe { font_face.GetWeight() };
    let style = unsafe { font_face.GetStyle() };
    Some(Font {
        family: family_name.into(),
        features: FontFeatures::default(),
        weight: font_weight_from_dwrite(weight),
        style: font_style_from_dwrite(style),
        fallbacks: None,
    })
}

// https://learn.microsoft.com/en-us/windows/win32/api/dwrite/ne-dwrite-dwrite_font_feature_tag
fn apply_font_features(
    direct_write_features: &IDWriteTypography,
    features: &FontFeatures,
) -> Result<()> {
    let tag_values = features.tag_value_list();

    // All of these features are enabled by default by DirectWrite.
    // If you want to (and can) peek into the source of DirectWrite
    let mut feature_liga = make_direct_write_feature("liga", 1);
    let mut feature_clig = make_direct_write_feature("clig", 1);
    let mut feature_calt = make_direct_write_feature("calt", 1);

    for (tag, value) in tag_values {
        if tag.as_str() == "liga" && *value == 0 {
            feature_liga.parameter = 0;
            continue;
        }
        if tag.as_str() == "clig" && *value == 0 {
            feature_clig.parameter = 0;
            continue;
        }
        if tag.as_str() == "calt" && *value == 0 {
            feature_calt.parameter = 0;
            continue;
        }

        unsafe {
            direct_write_features.AddFontFeature(make_direct_write_feature(tag, *value))?;
        }
    }
    unsafe {
        direct_write_features.AddFontFeature(feature_liga)?;
        direct_write_features.AddFontFeature(feature_clig)?;
        direct_write_features.AddFontFeature(feature_calt)?;
    }

    Ok(())
}

#[inline]
const fn make_direct_write_feature(feature_name: &str, parameter: u32) -> DWRITE_FONT_FEATURE {
    let tag = make_direct_write_tag(feature_name);
    DWRITE_FONT_FEATURE {
        nameTag: tag,
        parameter,
    }
}

#[inline]
const fn make_open_type_tag(tag_name: &str) -> u32 {
    let bytes = tag_name.as_bytes();
    debug_assert!(bytes.len() == 4);
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[inline]
const fn make_direct_write_tag(tag_name: &str) -> DWRITE_FONT_FEATURE_TAG {
    DWRITE_FONT_FEATURE_TAG(make_open_type_tag(tag_name))
}

#[inline]
fn get_name(string: IDWriteLocalizedStrings, locale: &HSTRING) -> Result<String> {
    let mut locale_name_index = 0u32;
    let mut exists = BOOL(0);
    unsafe { string.FindLocaleName(locale, &mut locale_name_index, &mut exists as _)? };
    if !exists.as_bool() {
        unsafe {
            string.FindLocaleName(
                DEFAULT_LOCALE_NAME,
                &mut locale_name_index as _,
                &mut exists as _,
            )?
        };
        anyhow::ensure!(exists.as_bool(), "No localised string for {locale}");
    }

    let name_length = unsafe { string.GetStringLength(locale_name_index) }? as usize;
    let mut name_vec = vec![0u16; name_length + 1];
    unsafe {
        string.GetString(locale_name_index, &mut name_vec)?;
    }

    Ok(String::from_utf16_lossy(&name_vec[..name_length]))
}

fn get_system_subpixel_rendering() -> bool {
    let mut value = c_uint::default();
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETFONTSMOOTHINGTYPE,
            0,
            Some((&mut value) as *mut c_uint as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS::default(),
        )
    };
    if result.log_err().is_some() {
        value == FE_FONTSMOOTHINGCLEARTYPE
    } else {
        true
    }
}

fn get_system_ui_font_name() -> SharedString {
    unsafe {
        let mut info: LOGFONTW = std::mem::zeroed();
        let font_family = if SystemParametersInfoW(
            SPI_GETICONTITLELOGFONT,
            std::mem::size_of::<LOGFONTW>() as u32,
            Some(&mut info as *mut _ as _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .log_err()
        .is_none()
        {
            // https://learn.microsoft.com/en-us/windows/win32/uxguide/vis-fonts
            // Segoe UI is the Windows font intended for user interface text strings.
            "Segoe UI".into()
        } else {
            let font_name = String::from_utf16_lossy(&info.lfFaceName);
            font_name.trim_matches(char::from(0)).to_owned().into()
        };
        log::info!("Use {} as UI font.", font_family);
        font_family
    }
}

// One would think that with newer DirectWrite method: IDWriteFontFace4::GetGlyphImageFormats
// but that doesn't seem to work for some glyphs, say ❤
fn is_color_glyph(
    font_face: &IDWriteFontFace3,
    glyph_id: GlyphId,
    factory: &IDWriteFactory5,
) -> bool {
    let glyph_run = DWRITE_GLYPH_RUN {
        fontFace: ManuallyDrop::new(Some(unsafe { std::ptr::read(&****font_face) })),
        fontEmSize: 14.0,
        glyphCount: 1,
        glyphIndices: &(glyph_id.0 as u16),
        glyphAdvances: &0.0,
        glyphOffsets: &DWRITE_GLYPH_OFFSET {
            advanceOffset: 0.0,
            ascenderOffset: 0.0,
        },
        isSideways: BOOL(0),
        bidiLevel: 0,
    };
    unsafe {
        factory.TranslateColorGlyphRun(
            Vector2::default(),
            &glyph_run as _,
            None,
            DWRITE_GLYPH_IMAGE_FORMATS_COLR
                | DWRITE_GLYPH_IMAGE_FORMATS_SVG
                | DWRITE_GLYPH_IMAGE_FORMATS_PNG
                | DWRITE_GLYPH_IMAGE_FORMATS_JPEG
                | DWRITE_GLYPH_IMAGE_FORMATS_PREMULTIPLIED_B8G8R8A8,
            DWRITE_MEASURING_MODE_NATURAL,
            None,
            0,
        )
    }
    .is_ok()
}

const DEFAULT_LOCALE_NAME: PCWSTR = windows::core::w!("en-US");

#[cfg(test)]
mod tests {
    use super::{DirectWriteState, DirectWriteTextSystem, GPUState, GlyphLayerTexture};
    use crate::direct_write::ClusterAnalyzer;
    use crate::directx_devices::DirectXDevices;
    use windows::Win32::Graphics::DirectWrite::{DWRITE_GLYPH_OFFSET, DWRITE_GLYPH_RUN};
    use anyhow::Result;
    use gpui::{
        DevicePixels, Font, PlatformTextSystem, RenderGlyphParams, Rgba, bounds, point, px, size,
    };
    use std::ffi::c_void;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
        D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    #[test]
    fn test_cluster_map() {
        let cluster_map = [0];
        let mut analyzer = ClusterAnalyzer::new(&cluster_map, 1);
        let next = analyzer.next();
        assert_eq!(next, Some((1, 1)));
        let next = analyzer.next();
        assert_eq!(next, None);

        let cluster_map = [0, 1, 2];
        let mut analyzer = ClusterAnalyzer::new(&cluster_map, 3);
        let next = analyzer.next();
        assert_eq!(next, Some((1, 1)));
        let next = analyzer.next();
        assert_eq!(next, Some((1, 1)));
        let next = analyzer.next();
        assert_eq!(next, Some((1, 1)));
        let next = analyzer.next();
        assert_eq!(next, None);
        // 👨‍👩‍👧‍👦👩‍💻
        let cluster_map = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 4, 4, 4];
        let mut analyzer = ClusterAnalyzer::new(&cluster_map, 5);
        let next = analyzer.next();
        assert_eq!(next, Some((11, 4)));
        let next = analyzer.next();
        assert_eq!(next, Some((5, 1)));
        let next = analyzer.next();
        assert_eq!(next, None);
        // 👩‍💻
        let cluster_map = [0, 0, 0, 0, 0];
        let mut analyzer = ClusterAnalyzer::new(&cluster_map, 1);
        let next = analyzer.next();
        assert_eq!(next, Some((5, 1)));
        let next = analyzer.next();
        assert_eq!(next, None);
    }

    /// Modified by ibis inc.: Segoe UI Emoji's glyphs have COLR version 1 paint trees on
    /// Windows 11; Direct2D draws them in colour, and a glyph with none is left to the
    /// version 0 layers.
    #[test]
    fn color_emoji_paint_tree_is_drawn_in_colour() -> Result<()> {
        unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED).ok().ok() };
        let devices = DirectXDevices::new()?;
        let text_system = DirectWriteTextSystem::new(&devices)?;
        let font_id = text_system.font_id(&Font { family: "Segoe UI Emoji".into(), ..Default::default() })?;
        let glyph_id = text_system.glyph_for_char(font_id, '🎈').expect("a balloon");
        let params = RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: px(48.0),
            subpixel_variant: point(0u8, 0u8),
            scale_factor: 1.5,
            is_emoji: true,
            subpixel_rendering: false,
            dilation: 0,
        };
        let bounds = text_system.glyph_raster_bounds(&params)?;
        let state = text_system.state.read();
        let font = &state.fonts[font_id.0];
        let glyph = [glyph_id.0 as u16];
        let advance = [bounds.size.width.0 as f32];
        let offset = [DWRITE_GLYPH_OFFSET {
            advanceOffset: -bounds.origin.x.0 as f32 / 1.5,
            ascenderOffset: bounds.origin.y.0 as f32 / 1.5,
        }];
        let run = DWRITE_GLYPH_RUN {
            fontFace: std::mem::ManuallyDrop::new(Some(unsafe { std::ptr::read(&***font.font_face) })),
            fontEmSize: 48.0,
            glyphCount: 1,
            glyphIndices: glyph.as_ptr(),
            glyphAdvances: advance.as_ptr(),
            glyphOffsets: offset.as_ptr(),
            isSideways: false.into(),
            bidiLevel: 0,
        };
        let pixels = DirectWriteState::rasterize_paint_tree(&text_system.components, &run, windows_numerics::Vector2::default(), 1.5, bounds.size)?
            .expect("Segoe UI Emoji has a paint tree for the balloon");
        let opaque = pixels.chunks(4).filter(|p| p[3] == 255).count();
        let coloured = pixels.chunks(4).filter(|p| p[3] > 0 && (p[0] != p[1] || p[1] != p[2])).count();
        assert!(opaque > pixels.len() / 4 / 4, "the glyph is drawn: {opaque} opaque texels");
        assert!(coloured > opaque / 2, "the glyph is in colour: {coloured} of {opaque}");
        Ok(())
    }

    #[test]
    fn color_emoji_rasterization_is_stable_across_batches() -> Result<()> {
        let devices = DirectXDevices::new()?;
        let text_system = DirectWriteTextSystem::new(&devices)?;

        let font = Font {
            family: "Segoe UI Emoji".into(),
            ..Default::default()
        };
        let font_id = text_system.font_id(&font)?;

        let mut params_list = Vec::new();
        for ch in ['🫠', '🥹', '🧗', '🏋', '🚀', '🥺'] {
            let Some(glyph_id) = text_system.glyph_for_char(font_id, ch) else {
                log::info!("no glyph found for {ch}");
                continue;
            };
            let params = RenderGlyphParams {
                font_id,
                glyph_id,
                font_size: px(48.0),
                subpixel_variant: point(0u8, 0u8),
                scale_factor: 1.0,
                is_emoji: true,
                subpixel_rendering: false,
                dilation: 0,
            };
            let raster_bounds = text_system.glyph_raster_bounds(&params)?;
            if raster_bounds.size.width.0 == 0 || raster_bounds.size.height.0 == 0 {
                log::info!("raster bounds are empty for {ch}");
                continue;
            }
            params_list.push((params, raster_bounds));
        }
        assert!(!params_list.is_empty());

        let first: Vec<_> = params_list
            .iter()
            .map(|(params, bounds)| text_system.rasterize_glyph(params, *bounds))
            .collect::<Result<_>>()?;

        // Churn the texture heap with further rasterization passes. If the color
        // compositing leaks leftover texture data (the render target is not cleared),
        // the second batch can pick up different contents and differ from the first.
        // With an explicit clear both batches are deterministic and identical.
        for _ in 0..3 {
            for (params, bounds) in &params_list {
                text_system.rasterize_glyph(params, *bounds)?;
            }
        }
        let second: Vec<_> = params_list
            .iter()
            .map(|(params, bounds)| text_system.rasterize_glyph(params, *bounds))
            .collect::<Result<_>>()?;

        assert_eq!(
            first, second,
            "color glyph rasterization changed between batches; \
             render target contents are leaking into the glyph bitmaps"
        );
        Ok(())
    }

    #[test]
    fn color_emoji_composites_over_cleared_texture() -> Result<()> {
        let devices = DirectXDevices::new()?;
        let gpu_state = GPUState::new(&devices)?;

        const SIZE: u32 = 32;
        // Seed the render target with solid red so that any texel which the
        // compositing pass fails to clear/overwrite remains identifiable after
        // the readback.
        let poison = {
            let mut v = vec![0u8; (SIZE * SIZE * 4) as usize];
            for pixel in v.chunks_exact_mut(4) {
                pixel[2] = 255;
                pixel[3] = 255;
            }
            v
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: SIZE,
            Height: SIZE,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: poison.as_ptr() as *const c_void,
            SysMemPitch: SIZE * 4,
            SysMemSlicePitch: 0,
        };
        let texture = unsafe {
            let mut texture = None;
            gpu_state
                .device
                .CreateTexture2D(&desc, Some(&initial_data), Some(&mut texture))?;
            texture.unwrap()
        };
        let render_target_view = unsafe {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv = None;
            gpu_state
                .device
                .CreateRenderTargetView(&texture, Some(&desc), Some(&mut rtv))?;
            rtv.unwrap()
        };

        // A single opaque layer in the top-left corner; the bottom-right corner
        // of the texture is covered by no layer at all.
        let layer_alpha = vec![255u8; 4 * 4];
        let layer = GlyphLayerTexture::new(
            &gpu_state,
            Rgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            bounds(point(0, 0), size(4, 4)),
            &layer_alpha,
        )?;

        let rasterized = DirectWriteState::composite_color_layers(
            &gpu_state,
            std::slice::from_ref(&layer),
            size(DevicePixels(SIZE as i32), DevicePixels(SIZE as i32)),
            &texture,
            &Some(render_target_view),
        )?;

        let corner = (SIZE as usize - 1 + (SIZE as usize - 1) * SIZE as usize) * 4;
        assert_eq!(
            &rasterized[corner..corner + 4],
            &[0, 0, 0, 0],
            "uncovered texel retained the poison from the uninitialized render target"
        );
        Ok(())
    }
}
