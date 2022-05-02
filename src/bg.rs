use std::{
    sync::{Arc, mpsc},
};
use ttf_parser::GlyphId;
use msdfgen::{Bitmap, RGB};
use rustybuzz::Face;

use super::FaceState;

enum BgCmd {
    AddFace {
        face_data: Arc<Vec<u8>>, face: Face<'static>,
        border_texels: f32, texels_per_em_x: f32, texels_per_em_y: f32
    },
    RenderGlyph {
        face_index: usize, glyph_id: GlyphId,
        atlas_w: u32, atlas_h: u32,
    },
}

pub(crate) struct Renderer {
    command_tx: mpsc::Sender<BgCmd>,
    glyph_rx: mpsc::Receiver<(usize, u16, f32, f32, f32, f32, u32, u32,
                              Bitmap<RGB<u8>>)>,
}

impl Renderer {
    pub fn new() -> Renderer {
        let (command_tx, command_rx) = mpsc::channel();
        let (glyph_tx, glyph_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("Psilo-Text BG glyph renderer".to_string())
            .spawn(move || {
                let mut faces = vec![];
                while let Ok(cmd) = command_rx.recv() {
                    match cmd {
                        BgCmd::AddFace { face_data, face, border_texels,
                                         texels_per_em_x,texels_per_em_y } => {
                            faces.push(FaceState {
                                _face_data: face_data, face, border_texels,
                                texels_per_em_x, texels_per_em_y,
                            });
                        },
                        BgCmd::RenderGlyph { face_index, glyph_id,
                                             atlas_w, atlas_h } => {
                            let face = faces.get(face_index)
                                .expect("Face index out of range? (This \
                                         should not happen, as our caller \
                                         should have bounds checked for us");
                            let res = face.render_glyph(glyph_id,
                                                        atlas_w, atlas_h);
                            if let Some((a,b,c,d,e,f,g)) = res {
                                let res = (face_index, glyph_id.0,
                                           a,b,c,d,e,f,g);
                                if glyph_tx.send(res).is_err() { break }
                            }
                        },
                    }
                }
            }).expect("Unable to spawn background glyph rendering thread");
        Renderer {
            command_tx, glyph_rx,
        }
    }
    pub fn add_face(&self, face_data: Arc<Vec<u8>>, face: Face<'static>,
                    border_texels: f32, texels_per_em_x: f32,
                    texels_per_em_y: f32) {
        self.command_tx
            .send(BgCmd::AddFace {
                face_data, face, border_texels,
                texels_per_em_x, texels_per_em_y,
            }).expect("background render thread died?");
    }
    pub fn render_glyph(&self, face_index: usize, glyph_id: GlyphId,
                        atlas_w: u32, atlas_h: u32) {
        self.command_tx
            .send(BgCmd::RenderGlyph {
                face_index, glyph_id, atlas_w, atlas_h,
            }).expect("background render thread died?");
    }
    pub fn next_rendered_glyph(&self)
        -> Option<(usize, u16, f32, f32, f32, f32, u32, u32,
                   Bitmap<RGB<u8>>)> {
            self.glyph_rx.try_recv().ok()
        }
}
