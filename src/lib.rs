use std::{
    collections::HashMap,
    rc::Rc,
    mem::transmute,
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
use log::warn;

/// Some type that gives you the information you need to render a particular
/// glyph image (given a particular atlas). Don't forget to account for the
/// half-pixel borders, if that applies to your situation!
pub trait AtlasCoords {
    type A : AtlasHandler;
    /// `atlas_*`: the coordinates of this glyph image in the atlas.
    /// `glyph_*`: the glyph's bounding box information, in ems.
    fn from_atlas_region(atlas_x: u32, atlas_y: u32,
                         atlas_w: u32, atlas_h: u32,
                         render_x_min: f32, render_y_min: f32,
                         render_x_max: f32, render_y_max: f32,
                         handler: &mut Self::A) -> Self;
}

pub trait AtlasHandler {
    type AtlasHandle : Copy;
    type AtlasCoords : Copy + AtlasCoords<A = Self>;
    type E;
    fn new_atlas(&mut self) -> Result<(Self::AtlasHandle, u32, u32), Self::E>;
    fn add_to_atlas(&mut self,
                    target_atlas: Self::AtlasHandle,
                    glyph_x: u32, glyph_y: u32,
                    glyph_width: u32, glyph_height: u32,
                    glyph_pixels: &[u8]) -> Result<(), Self::E>;
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
struct Rect {
    x: u32, y: u32, w: u32, h: u32,
}

struct AtlasState<A: AtlasHandler> {
    handle: A::AtlasHandle,
    packer: Packer,
}

impl<A: AtlasHandler> AtlasState<A> {
    pub fn new(handle: A::AtlasHandle, w: u32, h: u32) -> AtlasState<A> {
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

struct GlyphState<A: AtlasHandler> {
    atlas: u32,
    coords: A::AtlasCoords,
}

struct FaceState {
    /// This field is what `*_face` actually borrows from. `Rc` doesn't provide
    /// interior mutability, and without interior mutability the allocated
    /// block will never move, so this is *sound* (but not *safe*), as long as
    /// `*_face` is never moved out of us.
    _face_data: Rc<Vec<u8>>,
    face: Face<'static>,
    border_texels: f32,
    texels_per_em_x: f32,
    texels_per_em_y: f32,
}

pub struct TextHandler<A: AtlasHandler> {
    faces: Vec<FaceState>,
    atlases: Vec<AtlasState<A>>,
    glyphs: HashMap<u16, Option<GlyphState<A>>>,
}

impl<A: AtlasHandler> TextHandler<A> {
    pub fn new() -> TextHandler<A> {
        TextHandler {
            faces: Vec::new(),
            atlases: Vec::new(),
            glyphs: HashMap::new(),
        }
    }
    /// - `border_texels`: The number of texels of extra padding to put around
    ///   each SDF in the atlas for this face. When in doubt, use 4.0. This is
    ///   also the effective range of the SDF, so values less than 2.0 are
    ///   suicide!
    /// - `texels_per_em_*`: The number of texels that a single em in the given
    ///   font should occupy in the atlas. This should be experimentally
    ///   determined per font. 64 is usually a good starting point. Thinner
    ///   fonts will need higher values.
    pub fn add_face(&mut self, face_data: Rc<Vec<u8>>, index: u32,
                    border_texels: f32,
                    texels_per_em_x: f32, texels_per_em_y: f32)
        -> Option<usize> {
        let face = Face::from_slice(&face_data, index)?;
        let face: Face<'static> = unsafe { transmute(face) };
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
    pub fn get_glyph(&mut self, face: usize, glyph: u16, handler: &mut A)
        -> Result<Option<(A::AtlasHandle, A::AtlasCoords)>, A::E> {
        let mut err = None;
        let ret = self.glyphs.entry(glyph).or_insert_with(|| {
            // get the glyph from the font
            let face_state = self.faces.get_mut(face)
                .expect("Face index out of range");
            let face = &face_state.face;
            let mut shape = match face.glyph_shape(GlyphId(glyph)) {
                Some(x) => x,
                None => {
                    warn!("glyph {} appears to have no shape!", glyph);
                    return None;
                },
            };
            let bbox = match face.glyph_bounding_box(GlyphId(glyph)) {
                Some(bbox) => bbox,
                None => {
                    warn!("psilo-font only supports outline glyphs, but this \
                           font seems to contain an image glyph");
                    return None;
                }
            };
            let per_em = face.units_per_em() as f32;
            let raw_glyph_width = (bbox.x_max - bbox.x_min) as f32;
            let raw_glyph_height = (bbox.y_max - bbox.y_min) as f32;
            let glyph_width = raw_glyph_width
                * face_state.texels_per_em_x / per_em;
            let glyph_height = raw_glyph_height
                * face_state.texels_per_em_y / per_em;
            let sdf_width = (glyph_width + face_state.border_texels).ceil();
            let sdf_height = (glyph_height + face_state.border_texels).ceil();
            let wrangled_glyph_width = sdf_width - face_state.border_texels;
            let wrangled_glyph_height = sdf_height - face_state.border_texels;
            let sdf_width_int = sdf_width.ceil() as u32;
            let sdf_height_int = sdf_height.ceil() as u32;
            // font units -> sdf pixels
            let scale_x = wrangled_glyph_width / raw_glyph_width;
            let scale_y = wrangled_glyph_height / raw_glyph_height;
            let translate_x
                = face_state.border_texels / (scale_x * 2.0)
                - bbox.x_min as f32;
            let translate_y
                = face_state.border_texels / (scale_y * 2.0)
                - bbox.y_min as f32;
            let framing = msdfgen::Framing::new(
                face_state.border_texels as f64,
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

            // put it in the atlas
            let mut atlas_index = None;
            let mut outer_x = 0;
            let mut outer_y = 0;
            for (index, state) in self.atlases.iter_mut().enumerate() {
                if let Some((x, y)) = state.attempt_fit(sdf_width_int,
                                                        sdf_height_int) {
                    if let Err(e)
                        = handler.add_to_atlas(state.handle,
                                               x, y,
                                               sdf_width_int, sdf_height_int,
                                               bitmap.raw_pixels()) {
                            err = Some(e);
                            return None
                        }
                    atlas_index = Some(index);
                    outer_x = x;
                    outer_y = y;
                    break;
                }
            }
            let atlas_index = match atlas_index {
                None => {
                    // make a new atlas and add it to that
                    let (handle, w, h) = match handler
                        .new_atlas() {
                            Ok(x) => x,
                            Err(x) => {
                                err = Some(x);
                                return None;
                            }
                        };
                    self.atlases.push(AtlasState::new(handle, w, h));
                    let state = self.atlases.last_mut().unwrap();
                    if let Some((x, y)) = state.attempt_fit(sdf_width_int,
                                                            sdf_height_int) {
                        if let Err(e)
                            = handler.add_to_atlas(state.handle,
                                                   x, y,
                                                   sdf_width_int,
                                                   sdf_height_int,
                                                   bitmap.raw_pixels()) {
                                err = Some(e);
                                return None
                            }
                        outer_x = x;
                        outer_y = y;
                    }
                    self.atlases.len() - 1
                },
                Some(x) => x,
            };
            let atlas_x = outer_x;
            let atlas_y = outer_y;
            let atlas_w = sdf_width_int;
            let atlas_h = sdf_height_int;
            let half_extra_width = (sdf_width - glyph_width)
                / face_state.texels_per_em_x * 0.5;
            let half_extra_height = (sdf_height - glyph_height)
                / face_state.texels_per_em_y * 0.5;
            let render_x_min = bbox.x_min as f32 / per_em - half_extra_width;
            let render_y_min = bbox.y_min as f32 / per_em - half_extra_height;
            let render_x_max = bbox.x_max as f32 / per_em + half_extra_width;
            let render_y_max = bbox.y_max as f32 / per_em + half_extra_height;
            let coords = A::AtlasCoords
                ::from_atlas_region(atlas_x, atlas_y, atlas_w, atlas_h,
                                    render_x_min, render_y_min,
                                    render_x_max, render_y_max,
                                    handler);
            Some(GlyphState {
                atlas: atlas_index as u32,
                coords,
            })
        });
        if let Some(e) = err { Err(e) }
        else {
            Ok(ret.as_ref().map(|ret| (self.atlases[ret.atlas as usize].handle, ret.coords)))
        }
    }
}
