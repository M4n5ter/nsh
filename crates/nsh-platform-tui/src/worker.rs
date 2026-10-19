//! Graphics work stays off the terminal loop and the engine thread.

use std::{collections::HashMap, sync::Arc};

use crossbeam_channel::{Receiver, Sender, unbounded};
use nsh_animation::AnimationValue;
use nsh_core::{NodeId, access::Access, canvas::Drawing};

use crate::image::{AnimatedRaster, Raster};

enum Job {
    Image(String, Option<Arc<dyn nsh_engine::Assets>>, Access),
    Canvas(
        NodeId,
        Arc<Drawing>,
        Vec<(u16, AnimationValue)>,
        f32,
        f32,
        HashMap<Arc<str>, Arc<Raster>>,
    ),
    Protocol(NodeId, u64, Box<crate::protocol::Request>),
}

pub(crate) enum GraphicsResult {
    Image(String, Result<AnimatedRaster, String>),
    Canvas(NodeId, Result<Raster, String>),
    Protocol(NodeId, u64, Result<crate::protocol::Encoded, String>),
}

pub(crate) struct Graphics {
    tx: Sender<Job>,
    rx: Receiver<GraphicsResult>,
}

impl Graphics {
    pub(crate) fn new() -> Self {
        let (tx, jobs) = unbounded();
        let (results, rx) = unbounded();
        std::thread::Builder::new()
            .name("nsh-tui-graphics".to_owned())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let result = match job {
                        Job::Image(uri, assets, access) => {
                            let result = if !access.allows_media(&uri) {
                                Err("Image access was denied".to_owned())
                            } else if uri.starts_with("asset:/") {
                                assets
                                    .ok_or_else(|| "This app has no asset reader".to_owned())
                                    .and_then(|assets| assets.read(&uri))
                                    .and_then(|bytes| crate::image::decode_animation(&bytes))
                            } else {
                                crate::image::load_animation(&uri, None, &access)
                            };
                            GraphicsResult::Image(uri, result)
                        }
                        Job::Canvas(id, drawing, args, width, height, images) => {
                            GraphicsResult::Canvas(
                                id,
                                crate::canvas::rasterize(&drawing, &args, width, height, &images),
                            )
                        }
                        Job::Protocol(id, sequence, request) => GraphicsResult::Protocol(
                            id,
                            sequence,
                            crate::protocol::encode(&request),
                        ),
                    };
                    if results.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("start terminal graphics worker");
        Self { tx, rx }
    }

    pub(crate) fn image(
        &self,
        uri: String,
        assets: Option<Arc<dyn nsh_engine::Assets>>,
        access: Access,
    ) {
        let _ = self.tx.send(Job::Image(uri, assets, access));
    }
    pub(crate) fn canvas(
        &self,
        id: NodeId,
        drawing: Arc<Drawing>,
        args: Vec<(u16, AnimationValue)>,
        width: f32,
        height: f32,
        images: HashMap<Arc<str>, Arc<Raster>>,
    ) {
        let _ = self
            .tx
            .send(Job::Canvas(id, drawing, args, width, height, images));
    }
    pub(crate) fn poll(&self) -> Option<GraphicsResult> {
        self.rx.try_recv().ok()
    }

    pub(crate) fn protocol(&self, id: NodeId, sequence: u64, request: crate::protocol::Request) {
        let _ = self.tx.send(Job::Protocol(id, sequence, Box::new(request)));
    }
}
