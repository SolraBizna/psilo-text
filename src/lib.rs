//! This is a font glyph caching and consolidation layer for games. It's part
//! of the Psilo engine, but it doesn't depend on any other part of the engine.
//! Given some TTF outline fonts, and renderer-specific code for the actual
//! mechanics of the atlases, this layer provides multichannel signed distance
//! field generation and atlas packing. It does not precalculate the atlases,
//! instead populating them at runtime whenever glyphs are actually used. This
//! provides moderately worse packing efficiency, in exchange for increased
//! flexibility when rendering unexpected glyphs.
//!
//! Signed distance fields are a clever way to get decent-quality realtime text
//! rendering with low runtime cost. Multichannel signed distance fields, as
//! originated by Viktor Chlumský, are an even more clever way to greatly
//! improve the quality of the rendering with only a trivial increase in
//! rendering overhead and even a potential *reduction* in video memory
//! overhead. For more information, see [Chlumský's Master's thesis][1].
//!
//! [1]: https://github.com/Chlumsky/msdfgen/files/3050967/thesis.pdf

use std::{
    collections::{HashMap, hash_map::Entry},
    mem::transmute,
    sync::Arc,
};
use ttf_parser::GlyphId;
use msdfgen::{
    Bitmap,
    FontExt,
    EDGE_THRESHOLD,
    OVERLAP_SUPPORT,
    RGB,
};
use rect_packer::Packer;
use rustybuzz::Face;
use log::{error, warn};

#[cfg(feature="bg-render")]
mod bg;

pub trait AtlasHandler {
    type AtlasID : Copy;
    type AtlasCoords : Copy;
    type E;
    /// Create a new, blank atlas.
    fn new_atlas(&mut self) -> Result<Self::AtlasID, Self::E>;
    /// Return the size of the atlases that this handler will create. We call
    /// this a lot, so if determining this value is expensive, cache it!
    fn get_atlas_size(&mut self) -> (u32, u32);
    /// This function performs two operations:
    ///
    /// 1. Upload the given glyph pixels to the given region of the given
    ///    atlas.
    /// 2. Return an `AtlasCoords` that provide enough information to later
    ///    render this glyph.
    ///
    /// (Don't forget to account for the half-texel borders!)
    fn add_to_atlas(&mut self,
                    target_atlas: Self::AtlasID,
                    render_x_min: f32, render_y_min: f32,
                    render_x_max: f32, render_y_max: f32,
                    glyph_x: u32, glyph_y: u32,
                    glyph_width: u32, glyph_height: u32,
                    glyph_pixels: &[u8]) -> Result<Self::AtlasCoords, Self::E>;
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Rect {
    x: u32, y: u32, w: u32, h: u32,
}

struct AtlasState<AtlasID: Copy> {
    handle: AtlasID,
    packer: Packer,
}

impl<AtlasID: Copy> AtlasState<AtlasID> {
    pub fn new(handle: AtlasID, w: u32, h: u32) -> AtlasState<AtlasID>{
        AtlasState {
            handle,
            packer: Packer::new(rect_packer::Config {
                width: w as i32, height: h as i32,
                border_padding: 0, rectangle_padding: 0,
            }),
        }
    }
    pub fn attempt_fit(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        match self.packer.pack(w as i32, h as i32, false) {
            Some(rect) => Some((rect.x as u32, rect.y as u32)),
            None => None,
        }
    }
}

struct GlyphState<AtlasID: Copy, AtlasCoords: Copy> {
    atlas: AtlasID,
    coords: AtlasCoords,
}

struct FaceState {
    /// This field is what `*_face` actually borrows from. `Arc` doesn't provide
    /// interior mutability, and without interior mutability the allocated
    /// block will never move, so this is *sound* (but not *safe*), as long as
    /// `*_face` is never moved out of us.
    _face_data: Arc<Vec<u8>>,
    face: Face<'static>,
    border_texels: f32,
    texels_per_em_x: f32,
    texels_per_em_y: f32,
}

impl FaceState {
    /// Renders a glyph into an MSDF. Returns enough information to add the
    /// glyph to the atlas.
    ///
    /// Returned values are in the same order as they are passed to
    /// `add_to_atlas`, except that `atlas_x` and `atlas_y` are missing.
    ///
    /// Returns `None` if the given glyph is not present in the font, or if it
    /// has no actual shape.
    pub fn render_glyph(&self, glyph: GlyphId, atlas_w: u32, atlas_h: u32)
        -> Option<(f32, f32, f32, f32, u32, u32, Bitmap<RGB<u8>>)> {
        let mut shape = match self.face.glyph_shape(glyph) {
            Some(x) => x,
            None => {
                // This warning was more common than I thought, and not really
                // actionable.
                //warn!("glyph {} appears to have no shape!", glyph);
                return None;
            },
        };
        let bbox = match self.face.glyph_bounding_box(glyph) {
            Some(bbox) => bbox,
            None => {
                warn!("psilo-font only supports outline glyphs, but this \
                       font seems to contain an image glyph");
                return None;
            }
        };
        let per_em = self.face.units_per_em() as f32;
        let raw_glyph_width = (bbox.x_max - bbox.x_min) as f32;
        let raw_glyph_height = (bbox.y_max - bbox.y_min) as f32;
        let glyph_width = raw_glyph_width
            * self.texels_per_em_x / per_em;
        let glyph_height = raw_glyph_height
            * self.texels_per_em_y / per_em;
        let sdf_width = (glyph_width + self.border_texels).ceil();
        let sdf_height = (glyph_height + self.border_texels).ceil();
        let wrangled_glyph_width = sdf_width - self.border_texels;
        let wrangled_glyph_height = sdf_height - self.border_texels;
        let sdf_width_int = (sdf_width.ceil() as u32).min(atlas_w);
        let sdf_height_int = (sdf_height.ceil() as u32).min(atlas_h);
        // font units -> sdf pixels
        let scale_x = wrangled_glyph_width / raw_glyph_width;
        let scale_y = wrangled_glyph_height / raw_glyph_height;
        let translate_x
            = self.border_texels / (scale_x * 2.0)
            - bbox.x_min as f32;
        let translate_y
            = self.border_texels / (scale_y * 2.0)
            - bbox.y_min as f32;
        let framing = msdfgen::Framing::new(
            self.border_texels as f64 * 16.0,
            msdfgen::Vector2::new(scale_x as f64, scale_y as f64),
            msdfgen::Vector2::new(translate_x as f64, translate_y as f64),
        );

        let mut bitmap = Bitmap::new(sdf_width_int, sdf_height_int);

        // Is this still right?
        shape.edge_coloring_simple(3.0, 0);

        // render an SDF for it
        shape.generate_msdf(&mut bitmap, &framing,
                            EDGE_THRESHOLD, OVERLAP_SUPPORT);

        // convert to 24-bit RGB
        let bitmap: Bitmap<RGB<u8>> = bitmap.convert();

        let half_extra_width = (sdf_width - glyph_width)
            / self.texels_per_em_x * 0.5;
        let half_extra_height = (sdf_height - glyph_height)
            / self.texels_per_em_y * 0.5;
        let render_x_min = bbox.x_min as f32 / per_em - half_extra_width;
        let render_y_min = bbox.y_min as f32 / per_em - half_extra_height;
        let render_x_max = bbox.x_max as f32 / per_em + half_extra_width;
        let render_y_max = bbox.y_max as f32 / per_em + half_extra_height;
        Some((render_x_min, render_y_min,
              render_x_max, render_y_max,
              sdf_width_int, sdf_height_int,
              bitmap))
    }
}

enum GlyphStateInCache<AtlasID: Copy, AtlasCoords: Copy> {
    Null,
    #[cfg(feature="bg-render")]
    Pending,
    Present(GlyphState<AtlasID, AtlasCoords>),
}

impl<AtlasID: Copy, AtlasCoords: Copy> GlyphStateInCache<AtlasID, AtlasCoords> {
    #[cfg(feature="bg-render")]
    pub fn is_pending(&self) -> bool {
        match self {
            GlyphStateInCache::Pending => true,
            _ => false,
        }
    }
}

pub struct TextHandler<AtlasID: Copy, AtlasCoords: Copy> {
    faces: Vec<FaceState>,
    atlases: Vec<AtlasState<AtlasID>>,
    glyphs: HashMap<(usize, u16), GlyphStateInCache<AtlasID, AtlasCoords>>,
    #[cfg(feature="bg-render")]
    bg: bg::Renderer,
    #[cfg(feature="bg-render")]
    render_in_bg: bool,
}

impl<AtlasID: Copy, AtlasCoords: Copy> TextHandler<AtlasID, AtlasCoords> {
    pub fn new() -> TextHandler<AtlasID, AtlasCoords> {
        TextHandler {
            faces: Vec::new(),
            atlases: Vec::new(),
            glyphs: HashMap::new(),
            #[cfg(feature="bg-render")] bg: bg::Renderer::new(),
            #[cfg(feature="bg-render")] render_in_bg: true,
        }
    }
    /// Set whether new glyphs will be rendered in the background. When
    /// enabled, new glyphs will be invisible until they finish background
    /// rendering. When disabled, new glyphs will cause a noticeable hitch.
    /// Go with whichever of these is the lesser evil for you, or consider
    /// making it a runtime setting.
    ///
    /// Default is to render in the background.
    ///
    /// Any glyphs currently in the process of being background-rendered will
    /// continue being background-rendered regardless of the value of this
    /// option.
    ///
    /// This feature is controlled by the `bg-render` feature flag, which is
    /// *disabled* by default.
    #[cfg(feature="bg-render")]
    pub fn set_render_in_background(&mut self, nu: bool) {
        self.render_in_bg = nu;
    }
    /// - `border_texels`: The number of texels of extra padding to put around
    ///   each SDF in the atlas for this face. When in doubt, use 4.0. This is
    ///   also the effective range of the SDF, so values less than 2.0 are
    ///   suicide!
    /// - `texels_per_em_*`: The number of texels that a single em in the given
    ///   font should occupy in the atlas. This should be experimentally
    ///   determined per font. 64 is usually a good starting point. Thinner
    ///   fonts will need higher values.
    pub fn add_face(&mut self, face_data: Arc<Vec<u8>>, index: u32,
                    border_texels: f32,
                    texels_per_em_x: f32, texels_per_em_y: f32)
        -> Option<usize> {
        let face = Face::from_slice(&face_data, index)?;
        let face: Face<'static> = unsafe { transmute(face) };
        #[cfg(feature = "bg-render")] {
            self.bg.add_face(face_data.clone(), face.clone(),
                             border_texels, texels_per_em_x, texels_per_em_y);
        }
        self.faces.push(FaceState { _face_data: face_data, face, border_texels,
                                     texels_per_em_x, texels_per_em_y });
        Some(self.faces.len()-1)
    }
    pub fn get_face(&self, i: usize) -> Option<&Face> {
        // We need to massage the lifetime here. We have told the compiler that
        // this Face has `'static` lifetime, but in truth it is only valid as
        // long as we are. `transmute` will do the appropriate massaging.
        unsafe { transmute(self.faces.get(i).map(|x| &x.face)) }
    }
    pub fn get_face_mut(&mut self, i: usize) -> Option<&mut Face> {
        unsafe { transmute(self.faces.get_mut(i).map(|x| &mut x.face)) }
    }
    /// If the `bg-render` feature is enabled, this may render new glyphs in
    /// the background. The `bg-render` feature is *disabled* by default.
    pub fn get_glyph<A>(&mut self, face: usize, glyph: u16, handler: &mut A)
        -> Result<Option<(AtlasID, AtlasCoords)>, A::E>
    where A: AtlasHandler<AtlasID=AtlasID, AtlasCoords=AtlasCoords> {
        #[cfg(feature="bg-render")]
        while let Some((face, glyph, render_x_min, render_y_min,
                        render_x_max, render_y_max, sdf_width_int,
                        sdf_height_int, bitmap))
            = self.bg.next_rendered_glyph() {
                match self.glyphs.entry((face, glyph)) {
                    Entry::Vacant(_) => {
                        warn!("Glyph {} of face {}: rendered without us \
                               asking for it?", glyph, face);
                    },
                    Entry::Occupied(mut ent) => {
                        if ent.get().is_pending() {
                            let (atlas_w, atlas_h) = handler.get_atlas_size();
                            let res = put_into_atlas(&mut self.atlases,
                                                     handler, atlas_w, atlas_h,
                                                     render_x_min, render_y_min,
                                                     render_x_max, render_y_max,
                                                     sdf_width_int, sdf_height_int,
                                                     bitmap);
                            match res {
                                Ok(res) => {
                                    ent.insert(GlyphStateInCache::Present(res));
                                },
                                Err(_) => {
                                    error!("Error inserting background-rendered \
                                            glyph!",);
                                    ent.insert(GlyphStateInCache::Null);
                                }
                            }
                        }
                        else {
                            warn!("Glyph {} of face {}: rendered more than \
                                   once?", glyph, face);
                        }
                    },
                }
            }
        let mut err = None;
        let ret = self.glyphs.entry((face, glyph)).or_insert_with(|| {
            let render_in_bg;
            let (atlas_w, atlas_h) = handler.get_atlas_size();
            #[cfg(feature="bg-render")] { render_in_bg = self.render_in_bg; }
            #[cfg(not(feature="bg-render"))] { render_in_bg = false; }
            if render_in_bg {
                #[cfg(feature="bg-render")] {
                    self.bg.render_glyph(face, GlyphId(glyph),
                                         atlas_w, atlas_h);
                    GlyphStateInCache::Pending
                }
                #[cfg(not(feature="bg-render"))] {
                    unreachable!()
                }
            }
            else {
                // get the glyph from the font
                let face_state = self.faces.get_mut(face)
                    .expect("Face index out of range");
                let (render_x_min, render_y_min, render_x_max, render_y_max,
                     sdf_width_int, sdf_height_int, bitmap)
                    = match face_state.render_glyph(GlyphId(glyph),
                                                    atlas_w, atlas_h) {
                        None => return GlyphStateInCache::Null,
                        Some(x) => x,
                    };
                let res = put_into_atlas(&mut self.atlases,
                                         handler, atlas_w, atlas_h,
                                         render_x_min, render_y_min,
                                         render_x_max, render_y_max,
                                         sdf_width_int, sdf_height_int,
                                         bitmap);
                match res {
                    Ok(res) => GlyphStateInCache::Present(res),
                    Err(x) => {
                        err = Some(x);
                        GlyphStateInCache::Null
                    }
                }
            }
        });
        if let Some(e) = err { Err(e) }
        else {
            Ok(match &ret {
                GlyphStateInCache::Null => None,
                #[cfg(feature="bg-render")]
                GlyphStateInCache::Pending => None,
                GlyphStateInCache::Present(ret)
                    => Some((ret.atlas, ret.coords)),
            })
        }
    }
}

fn put_into_atlas<A, AtlasID: Copy, AtlasCoords: Copy>
    (atlases: &mut Vec<AtlasState<AtlasID>>, handler: &mut A,
     atlas_w: u32, atlas_h: u32,
     render_x_min: f32, render_y_min: f32,
     render_x_max: f32, render_y_max: f32,
     sdf_width_int: u32, sdf_height_int: u32,
     bitmap: Bitmap<RGB<u8>>)
    -> Result<GlyphState<AtlasID, AtlasCoords>, A::E>
where A: AtlasHandler<AtlasID=AtlasID, AtlasCoords=AtlasCoords> {
    // put it in the atlas
    let mut fit = None;
    for state in atlases.iter_mut() {
        if let Some((x, y)) = state.attempt_fit(sdf_width_int,
                                                sdf_height_int) {
            fit = Some((state.handle, x, y));
            break;
        }
    }
    let (atlas_handle, atlas_x, atlas_y) = match fit {
        Some(x) => x,
        None => {
            let handle = handler.new_atlas()?;
            atlases.push(AtlasState::new(handle,
                                              atlas_w, atlas_h));
            let state = atlases.last_mut().unwrap();
            if let Some((x, y)) = state.attempt_fit(sdf_width_int,
                                                    sdf_height_int) {
                (state.handle, x, y)
            }
            else {
                // We have made sure that sdf_width_int and
                // sdf_height_int are at least as large as our atlases.
                // This case will never arise.
                unreachable!();
            }
        },
    };
    let coords = handler.add_to_atlas(atlas_handle,
                                      render_x_min, render_y_min,
                                      render_x_max, render_y_max,
                                      atlas_x, atlas_y,
                                      sdf_width_int, sdf_height_int,
                                      bitmap.raw_pixels())?;
    Ok(GlyphState {
        atlas: atlas_handle,
        coords,
    })
}
