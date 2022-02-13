use std::{
    collections::HashMap,
};
use freetype::Face;
use msdfgen::{
    Bitmap,
    FontExt,
    EDGE_THRESHOLD,
    OVERLAP_SUPPORT,
    RGB,
};
use rect_packer::Packer;

/// Some type that gives you the information you need to render a particular
/// glyph image (given a particular atlas). Don't forget to half-pixel it.
pub trait AtlasCoords {
    type A : AtlasHandler;
    fn from_atlas_region(x: u32, y: u32, w: u32, h: u32,
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
    face: Face,
    border_texels: f32,
    texels_per_em_x: f32,
    texels_per_em_y: f32,
}

pub struct Font<A: AtlasHandler> {
    faces: Vec<FaceState>,
    atlases: Vec<AtlasState<A>>,
    glyphs: HashMap<u32, Option<GlyphState<A>>>,
}

impl<A: AtlasHandler> Font<A> {
    pub fn new() -> Font<A> {
        Font { faces: Vec::new(), atlases: Vec::new(), glyphs: HashMap::new() }
    }
    /// `border_texels`: The number of texels of extra padding to put around
    /// each SDF in the atlas for this face. When in doubt, use 4.0.
    /// `texels_per_em_*`: The number of texels that a single em in the given
    /// font should occupy in the atlas. This should be experimentally
    /// determined per font. 64 is usually a good starting point. Thinner fonts
    /// will need higher values.
    pub fn add_face(&mut self, face: Face, border_texels: f32,
                    texels_per_em_x: f32, texels_per_em_y: f32) -> usize {
        self.faces.push(FaceState { face, border_texels,
                                    texels_per_em_x, texels_per_em_y });
        self.faces.len()-1
    }
    pub fn get_face(&self, i: usize) -> Option<&Face> {
        self.faces.get(i).map(|x| &x.face)
    }
    pub fn get_face_mut(&mut self, i: usize) -> Option<&mut Face> {
        self.faces.get_mut(i).map(|x| &mut x.face)
    }
    pub fn get_glyph(&mut self, face: usize, glyph: u32, handler: &mut A)
        -> Result<Option<(A::AtlasHandle, A::AtlasCoords)>, A::E> {
        let mut err = None;
        let ret = self.glyphs.entry(glyph).or_insert_with(|| {
            // get the glyph from the font
            let face_state = self.faces.get_mut(face)
                .expect("Face index out of range");
            let face = &face_state.face;
            let mut shape = face.glyph_shape(glyph).unwrap();
            let cbox = face.glyph().get_glyph().unwrap().get_cbox(0);
            let glyph_width = cbox.xMax - cbox.xMin;
            let glyph_height = cbox.yMax - cbox.yMin;
            let sdf_width = glyph_width as f32 * face_state.texels_per_em_x
                / face.em_size() as f32 + face_state.border_texels;
            let sdf_height = glyph_height as f32 * face_state.texels_per_em_y
                / face.em_size() as f32 + face_state.border_texels;
            let sdf_width = sdf_width.ceil() as u32;
            let sdf_height = sdf_height.ceil() as u32;
            let framing = match shape
                .get_bounds()
                .autoframe(sdf_width, sdf_height,
                           msdfgen::Range::Px(face_state.border_texels as f64),
                           None) {
                    None => { return None },
                    Some(x) => x,
                };

            let mut bitmap = Bitmap::new(sdf_width, sdf_height);

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
                if let Some((x, y)) = state.attempt_fit(sdf_width, sdf_height){
                    if let Err(e)
                        = handler.add_to_atlas(state.handle,
                                               x, y, sdf_width, sdf_height,
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
                    if let Some((x, y)) = state.attempt_fit(sdf_width,
                                                            sdf_height) {
                        if let Err(e)
                            = handler.add_to_atlas(state.handle,
                                                   x, y, sdf_width, sdf_height,
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
            Some(GlyphState {
                atlas: atlas_index as u32,
                coords: A::AtlasCoords::from_atlas_region(outer_x, outer_y,
                                                          sdf_width,sdf_height,
                                                          handler)
            })
        });
        if let Some(e) = err { Err(e) }
        else {
            Ok(ret.as_ref().map(|ret| (self.atlases[ret.atlas as usize].handle, ret.coords)))
        }
    }
}
