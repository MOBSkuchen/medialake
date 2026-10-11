//! The embedding engine: one thread owns the llama.cpp context; jobs arrive through a two-level
//! priority queue so that search queries are never stuck behind bulk indexing work.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use medialake_llama::{Embedder, Input, Params, Pooling};
use medialake_proto::models::Model;
use medialake_proto::{Embedding, TextItem, TextKind, format_text, media_prompt};
use tokio::sync::oneshot;

pub enum Work {
    Text(String),
    Images(Vec<Vec<u8>>),
    Audio(Vec<f32>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Priority {
    Query,
    Bulk,
}

struct Job {
    items: Vec<Work>,
    reply: oneshot::Sender<Vec<Embedding>>,
}

#[derive(Default)]
struct Queues {
    high: VecDeque<Job>,
    low: VecDeque<Job>,
    closed: bool,
}

struct Shared {
    q: Mutex<Queues>,
    cv: Condvar,
}

#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
    pub audio_sample_rate: u32,
    pub supports_audio: bool,
}

/// The llama.cpp settings that go with a model of the manifest.
pub fn llama_params(model: Model) -> Params {
    let (media_prefix, media_suffix) = media_prompt(model);
    let pooling = match model.pooling() {
        medialake_proto::models::Pooling::Mean => Pooling::Mean,
        medialake_proto::models::Pooling::Last => Pooling::Last,
    };
    Params { pooling, media_prefix, media_suffix }
}

impl Engine {
    /// Loads the model (blocking) and starts the worker thread.
    pub fn start(model_id: Model, model: &Path, mmproj: &Path, gpu_layers: i32, verbose: bool) -> Result<Engine, String> {
        let mut embedder = Embedder::load(model, mmproj, gpu_layers, verbose, llama_params(model_id))?;
        if embedder.dims() != model_id.dims() {
            return Err(format!("{} yields {}-dim vectors, expected {} for {}", model.display(), embedder.dims(), model_id.dims(), model_id.as_str()));
        }
        warm_up(&mut embedder, model_id);
        let audio_sample_rate = embedder.audio_sample_rate();
        let supports_audio = embedder.supports_audio();
        let shared = Arc::new(Shared { q: Mutex::new(Queues::default()), cv: Condvar::new() });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("embedder".into())
            .spawn(move || worker(embedder, s))
            .map_err(|e| e.to_string())?;
        Ok(Engine { shared, audio_sample_rate, supports_audio })
    }

    pub async fn embed(&self, priority: Priority, items: Vec<Work>) -> Vec<Embedding> {
        if items.is_empty() {
            return vec![];
        }
        let n = items.len();
        let (tx, rx) = oneshot::channel();
        {
            let mut q = self.shared.q.lock().unwrap();
            let job = Job { items, reply: tx };
            match priority {
                Priority::Query => q.high.push_back(job),
                Priority::Bulk => q.low.push_back(job),
            }
        }
        self.shared.cv.notify_one();
        rx.await.unwrap_or_else(|_| vec![Embedding::Failed("engine stopped".into()); n])
    }

    pub fn close(&self) {
        self.shared.q.lock().unwrap().closed = true;
        self.shared.cv.notify_all();
    }
}

fn run_item(e: &mut Embedder, w: &Work) -> Embedding {
    let r = match w {
        Work::Text(t) => e.embed(&Input::Text(t)),
        Work::Images(imgs) => {
            let refs: Vec<&[u8]> = imgs.iter().map(|v| v.as_slice()).collect();
            e.embed(&Input::Images(&refs))
        }
        Work::Audio(s) => e.embed(&Input::Audio(s)),
    };
    match r {
        Ok(v) => Embedding::Vector(v),
        Err(err) => Embedding::Failed(err),
    }
}

fn worker(mut embedder: Embedder, shared: Arc<Shared>) {
    loop {
        let job = {
            let mut q = shared.q.lock().unwrap();
            loop {
                if let Some(j) = q.high.pop_front() {
                    break j;
                }
                if let Some(j) = q.low.pop_front() {
                    break j;
                }
                if q.closed {
                    return;
                }
                q = shared.cv.wait(q).unwrap();
            }
        };
        let mut out = Vec::with_capacity(job.items.len());
        for w in &job.items {
            // a waiting query goes first, even in the middle of a bulk job
            loop {
                let hi = shared.q.lock().unwrap().high.pop_front();
                let Some(hj) = hi else { break };
                let res: Vec<Embedding> = hj.items.iter().map(|w| run_item(&mut embedder, w)).collect();
                let _ = hj.reply.send(res);
            }
            out.push(run_item(&mut embedder, w));
        }
        let _ = job.reply.send(out);
    }
}

/// 64x64 24-bit BMP (stb_image reads BMP): enough to compile GPU shaders / warm caches.
fn tiny_bmp() -> Vec<u8> {
    let (w, h) = (64u32, 64u32);
    let data = w * 3 * h;
    let mut b = Vec::with_capacity(54 + data as usize);
    b.extend_from_slice(b"BM");
    b.extend_from_slice(&(54 + data).to_le_bytes());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&54u32.to_le_bytes());
    b.extend_from_slice(&40u32.to_le_bytes());
    b.extend_from_slice(&w.to_le_bytes());
    b.extend_from_slice(&h.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&24u16.to_le_bytes());
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&data.to_le_bytes());
    b.extend_from_slice(&[0; 16]);
    for i in 0..data {
        b.push(((i * 7) % 251) as u8);
    }
    b
}

fn warm_up(e: &mut Embedder, model: Model) {
    let t = Instant::now();
    let text = format_text(model, TextKind::Document, &TextItem { title: String::new(), text: "warm-up".into() });
    let _ = e.embed(&Input::Text(&text));
    let bmp = tiny_bmp();
    let _ = e.embed(&Input::Images(&[&bmp]));
    if e.supports_audio() {
        let silence = vec![0.0f32; e.audio_sample_rate() as usize];
        let _ = e.embed(&Input::Audio(&silence));
    }
    tracing::info!("warm-up took {} ms", t.elapsed().as_millis());
}
