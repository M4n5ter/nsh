//! Graphics work stays off the terminal loop and the engine thread. In-memory
//! image sources keep their shared bytes through the queue; digest URIs remain
//! cache keys rather than locations to read.

use std::{collections::HashMap, sync::Arc};

use crossbeam_channel::{Receiver, Sender, unbounded};
use nsh_core::{
    NodeId, access::Access, animation::AnimationValue, canvas::Drawing, image::ImageData,
};

use crate::image::{AnimatedRaster, Raster};

enum Job {
    Blur(
        NodeId,
        crate::image_state::ImageRequest,
        Arc<AnimatedRaster>,
    ),
    Canvas(CanvasRequest),
    Protocol(NodeId, u64, Box<crate::protocol::Request>),
    Composite(NodeId, u64, Arc<crate::composite::Scene>),
}

type ImageJob = (
    String,
    Option<ImageData>,
    Option<Arc<dyn nsh_engine::Assets>>,
    Access,
);

pub(crate) struct CanvasRequest {
    pub(crate) id: NodeId,
    pub(crate) generation: u64,
    pub(crate) epoch: u64,
    pub(crate) drawing: Arc<Drawing>,
    pub(crate) args: Vec<(u16, AnimationValue)>,
    pub(crate) size: nsh_core::Size<nsh_core::Points>,
    pub(crate) images: HashMap<Arc<str>, Arc<Raster>>,
}

pub(crate) enum GraphicsResult {
    Image(String, Result<AnimatedRaster, String>),
    Progress(String, u64, u64),
    Blur(
        NodeId,
        crate::image_state::ImageRequest,
        Result<Arc<AnimatedRaster>, String>,
    ),
    Canvas(NodeId, u64, u64, Result<Raster, String>),
    Protocol(NodeId, u64, Result<crate::protocol::Encoded, String>),
    Composite(NodeId, u64, Result<Raster, String>),
}

pub(crate) struct Graphics {
    tx: Sender<Job>,
    image_tx: Sender<ImageJob>,
    rx: Receiver<GraphicsResult>,
}

impl Graphics {
    pub(crate) fn new(fonts: crate::fonts::FontCatalog) -> Self {
        let (tx, jobs) = unbounded();
        let (results, rx) = unbounded();
        let (image_tx, image_jobs) = unbounded::<ImageJob>();
        // IO cannot hold up a placeholder, Canvas or protocol encoder.
        // Two readers let a small placeholder finish beside its main image.
        for index in 0..2 {
            let jobs = image_jobs.clone();
            let results = results.clone();
            std::thread::Builder::new()
                .name(format!("nsh-tui-image-{index}"))
                .spawn(move || {
                    while let Ok((uri, data, assets, access)) = jobs.recv() {
                        let mut progress = |loaded, total| {
                            let _ =
                                results.send(GraphicsResult::Progress(uri.clone(), loaded, total));
                        };
                        let result = if !access.allows_media(&uri) {
                            Err("Image access was denied".to_owned())
                        } else if let Some(data) = data {
                            // The core normalizes data:/blob: sources to a
                            // digest URI. Their shared bytes travel with the
                            // request and take the same bounded decoder path.
                            progress(data.len() as u64, data.len() as u64);
                            crate::image::decode_animation(&data)
                        } else if uri.starts_with("asset:/") {
                            assets
                                .ok_or_else(|| "This app has no asset reader".to_owned())
                                .and_then(|assets| assets.read(&uri))
                                .and_then(|bytes| {
                                    progress(bytes.len() as u64, bytes.len() as u64);
                                    crate::image::decode_animation(&bytes)
                                })
                        } else {
                            crate::image::load_animation_with_progress(
                                &uri,
                                None,
                                &access,
                                &mut progress,
                            )
                        };
                        if results.send(GraphicsResult::Image(uri, result)).is_err() {
                            break;
                        }
                    }
                })
                .expect("start terminal image reader");
        }
        std::thread::Builder::new()
            .name("nsh-tui-graphics".to_owned())
            .spawn(move || {
                let mut blurs = crate::image::BlurCache::default();
                while let Ok(job) = jobs.recv() {
                    let result = match job {
                        Job::Blur(id, request, animation) => {
                            let radii = request.blur_radii(&animation);
                            GraphicsResult::Blur(id, request, blurs.blur(&animation, radii))
                        }
                        Job::Canvas(CanvasRequest {
                            id,
                            generation,
                            epoch,
                            drawing,
                            args,
                            size,
                            images,
                        }) => GraphicsResult::Canvas(
                            id,
                            generation,
                            epoch,
                            crate::canvas::rasterize(
                                &drawing,
                                &args,
                                size.width,
                                size.height,
                                &images,
                                &fonts,
                            ),
                        ),
                        Job::Protocol(id, sequence, request) => GraphicsResult::Protocol(
                            id,
                            sequence,
                            crate::protocol::encode(&request),
                        ),
                        Job::Composite(id, sequence, scene) => GraphicsResult::Composite(
                            id,
                            sequence,
                            crate::composite::rasterize(&scene, &fonts),
                        ),
                    };
                    if results.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("start terminal graphics worker");
        Self { tx, image_tx, rx }
    }

    pub(crate) fn image(
        &self,
        uri: String,
        data: Option<ImageData>,
        assets: Option<Arc<dyn nsh_engine::Assets>>,
        access: Access,
    ) {
        let _ = self.image_tx.send((uri, data, assets, access));
    }
    pub(crate) fn canvas(&self, request: CanvasRequest) {
        let _ = self.tx.send(Job::Canvas(request));
    }
    pub(crate) fn poll(&self) -> Option<GraphicsResult> {
        self.rx.try_recv().ok()
    }

    pub(crate) fn protocol(&self, id: NodeId, sequence: u64, request: crate::protocol::Request) {
        let _ = self.tx.send(Job::Protocol(id, sequence, Box::new(request)));
    }

    pub(crate) fn blur(
        &self,
        id: NodeId,
        request: crate::image_state::ImageRequest,
        animation: Arc<AnimatedRaster>,
    ) {
        let _ = self.tx.send(Job::Blur(id, request, animation));
    }

    pub(crate) fn composite(
        &self,
        id: NodeId,
        sequence: u64,
        scene: Arc<crate::composite::Scene>,
    ) -> bool {
        self.tx.send(Job::Composite(id, sequence, scene)).is_ok()
    }
}
