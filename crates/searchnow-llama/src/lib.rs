//! Thin safe wrapper over the raw llama.cpp / libmtmd C API for EmbeddingGemma 2.
//!
//! Text tokens and media embeddings must go through ONE decode call (one `llama_batch_ext`): mean
//! pooling only covers a single decode, so `mtmd_helper_eval_chunks` (which decodes chunk by chunk)
//! would yield a vector of only the trailing marker tokens.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;

use llama_cpp_sys_2 as sys;

pub const N_CTX: u32 = 8192;
pub const DIMS: usize = 768;

pub type Result<T> = std::result::Result<T, String>;

/// One embedding input.
pub enum Input<'a> {
    Text(&'a str),
    /// Encoded images (jpeg/png/...); several images are embedded together into ONE vector (video frames).
    Images(&'a [&'a [u8]]),
    /// Mono f32 PCM at [`Embedder::audio_sample_rate`].
    Audio(&'a [f32]),
}

pub struct Embedder {
    model: *mut sys::llama_model,
    ctx: *mut sys::llama_context,
    mtmd: *mut sys::mtmd_context,
}

// The raw handles are only ever used from one thread at a time (the owner holds the Embedder).
unsafe impl Send for Embedder {}

unsafe extern "C" fn quiet_log(_level: sys::ggml_log_level, _text: *const c_char, _ud: *mut c_void) {}

unsafe extern "C" fn stderr_log(_level: sys::ggml_log_level, text: *const c_char, _ud: *mut c_void) {
    if !text.is_null() {
        eprint!("{}", unsafe { CStr::from_ptr(text) }.to_string_lossy());
    }
}

impl Embedder {
    /// `gpu_layers`: 0 = CPU only, any other value offloads everything (the backend must support it).
    pub fn load(model_path: &Path, mmproj_path: &Path, gpu_layers: i32, verbose: bool) -> Result<Self> {
        unsafe {
            sys::llama_backend_init();
            let log: sys::ggml_log_callback = Some(if verbose { stderr_log } else { quiet_log });
            sys::llama_log_set(log, ptr::null_mut());
            sys::mtmd_helper_log_set(log, ptr::null_mut());

            let mut mp = sys::llama_model_default_params();
            mp.n_gpu_layers = gpu_layers;
            let path = cstr(model_path)?;
            let model = sys::llama_model_load_from_file(path.as_ptr(), mp);
            if model.is_null() {
                return Err(format!("failed to load model {}", model_path.display()));
            }

            let mut cp = sys::llama_context_default_params();
            cp.n_ctx = N_CTX;
            cp.n_batch = N_CTX;
            cp.n_ubatch = N_CTX; // non-causal model: the whole sequence in one ubatch
            cp.embeddings = true;
            cp.pooling_type = sys::LLAMA_POOLING_TYPE_MEAN;
            let ctx = sys::llama_init_from_model(model, cp);
            if ctx.is_null() {
                sys::llama_model_free(model);
                return Err("failed to create llama context".into());
            }

            let mut mtp = sys::mtmd_context_params_default();
            mtp.use_gpu = gpu_layers != 0;
            mtp.print_timings = false;
            let mm = cstr(mmproj_path)?;
            let mtmd = sys::mtmd_init_from_file(mm.as_ptr(), model, mtp);
            if mtmd.is_null() {
                sys::llama_free(ctx);
                sys::llama_model_free(model);
                return Err(format!("failed to load projector {}", mmproj_path.display()));
            }
            Ok(Self { model, ctx, mtmd })
        }
    }

    pub fn audio_sample_rate(&self) -> u32 {
        let r = unsafe { sys::mtmd_get_audio_sample_rate(self.mtmd) };
        if r > 0 { r as u32 } else { 16_000 }
    }

    pub fn supports_audio(&self) -> bool {
        unsafe { sys::mtmd_support_audio(self.mtmd) }
    }

    /// Embeds one input and returns the L2-normalised 768-dim vector.
    pub fn embed(&mut self, input: &Input) -> Result<Vec<f32>> {
        unsafe {
            let marker = CStr::from_ptr(sys::mtmd_default_marker()).to_str().unwrap().to_owned();
            let mut bitmaps: Vec<*mut sys::mtmd_bitmap> = vec![];
            let result = self.embed_inner(input, &marker, &mut bitmaps);
            for b in bitmaps {
                sys::mtmd_bitmap_free(b);
            }
            result
        }
    }

    unsafe fn embed_inner(&mut self, input: &Input, marker: &str, bitmaps: &mut Vec<*mut sys::mtmd_bitmap>) -> Result<Vec<f32>> {
        unsafe {
            // 1. tokenize text (+ media markers) into chunks
            let text = match input {
                Input::Text(t) => CString::new(t.replace('\0', " ")).unwrap(),
                Input::Images(imgs) => {
                    if imgs.is_empty() {
                        return Err("no images".into());
                    }
                    for img in imgs.iter() {
                        let w = sys::mtmd_helper_bitmap_init_from_buf(self.mtmd, img.as_ptr(), img.len(), false, sys::mtmd_helper_init_opt_default());
                        let b = w.bitmap;
                        if b.is_null() {
                            return Err("cannot decode image".into());
                        }
                        bitmaps.push(b);
                    }
                    CString::new(marker.repeat(imgs.len())).unwrap()
                }
                Input::Audio(samples) => {
                    if samples.is_empty() {
                        return Err("empty audio".into());
                    }
                    let b = sys::mtmd_bitmap_init_from_audio(samples.len(), samples.as_ptr());
                    if b.is_null() {
                        return Err("cannot create audio bitmap".into());
                    }
                    bitmaps.push(b);
                    CString::new(marker).unwrap()
                }
            };
            let it = sys::mtmd_input_text { text: text.as_ptr(), text_len: text.as_bytes().len(), add_special: true, parse_special: true };
            let chunks = sys::mtmd_input_chunks_init();
            let cb: Vec<*const sys::mtmd_bitmap> = bitmaps.iter().map(|b| *b as *const _).collect();
            let rc = sys::mtmd_tokenize(self.mtmd, chunks, &it, cb.as_ptr(), cb.len());
            if rc != 0 {
                sys::mtmd_input_chunks_free(chunks);
                return Err(format!("tokenize failed ({rc})"));
            }
            let total = sys::mtmd_helper_get_n_tokens(chunks);
            if total > N_CTX as usize {
                sys::mtmd_input_chunks_free(chunks);
                return Err(format!("input too long: {total} tokens > {N_CTX}"));
            }

            // 2. build ONE batch: text tokens + encoded media rows, sequential positions
            let n_embd_inp = sys::llama_model_n_embd_inp(self.model) as usize;
            let batch = sys::llama_batch_ext_init(self.ctx);
            let mut media_bufs: Vec<Vec<f32>> = vec![]; // keep encoder output alive until decode
            let mut pos: sys::llama_pos = 0;
            let mut err: Option<String> = None;
            'chunks: for ci in 0..sys::mtmd_input_chunks_size(chunks) {
                let chunk = sys::mtmd_input_chunks_get(chunks, ci);
                if sys::mtmd_input_chunk_get_type(chunk) == sys::MTMD_INPUT_CHUNK_TYPE_TEXT {
                    let mut n = 0usize;
                    let toks = sys::mtmd_input_chunk_get_tokens_text(chunk, &mut n);
                    for &tok in std::slice::from_raw_parts(toks, n) {
                        let idx = sys::llama_batch_ext_add_token(batch, 0, tok);
                        if idx < 0 {
                            err = Some("batch full".into());
                            break 'chunks;
                        }
                        finish_entry(batch, idx, &mut pos);
                    }
                } else {
                    if sys::mtmd_encode_chunk(self.mtmd, chunk) != 0 {
                        err = Some("media encoding failed".into());
                        break;
                    }
                    let n = sys::mtmd_input_chunk_get_n_tokens(chunk);
                    let out = sys::mtmd_get_output_embd(self.mtmd);
                    media_bufs.push(std::slice::from_raw_parts(out, n * n_embd_inp).to_vec());
                    let buf = media_bufs.last().unwrap();
                    for i in 0..n {
                        let row = sys::llama_embd { data: buf.as_ptr().add(i * n_embd_inp), n_rows: 1, n_embd: n_embd_inp };
                        let idx = sys::llama_batch_ext_add_embd(batch, 0, row);
                        if idx < 0 {
                            err = Some(format!("batch full / bad embedding ({idx})"));
                            break 'chunks;
                        }
                        finish_entry(batch, idx, &mut pos);
                    }
                }
            }

            // 3. single decode, read the pooled sequence embedding, L2-normalise
            let out = match err {
                Some(e) => Err(e),
                None => {
                    sys::llama_memory_clear(sys::llama_get_memory(self.ctx), true);
                    let rc = sys::llama_process(self.ctx, sys::LLAMA_PROCESS_TYPE_DECODE, batch);
                    if rc != 0 {
                        Err(format!("decode failed ({rc})"))
                    } else {
                        let n_out = sys::llama_model_n_embd_out(self.model) as usize;
                        let e = sys::llama_get_embeddings_seq(self.ctx, 0);
                        if e.is_null() {
                            Err("no pooled embedding".into())
                        } else {
                            let mut v = std::slice::from_raw_parts(e, n_out).to_vec();
                            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                            if !norm.is_finite() || norm == 0.0 {
                                Err("non-finite embedding".into())
                            } else {
                                v.iter_mut().for_each(|x| *x /= norm);
                                Ok(v)
                            }
                        }
                    }
                }
            };
            sys::llama_batch_ext_free(batch);
            sys::mtmd_input_chunks_free(chunks);
            out
        }
    }
}

unsafe fn finish_entry(batch: *mut sys::llama_batch_ext, idx: i32, pos: &mut sys::llama_pos) {
    unsafe {
        // Gemma is not M-RoPE: one position per entry, same value in every section
        let p = [*pos; 4];
        sys::llama_batch_ext_set_pos(batch, idx, p.as_ptr());
        sys::llama_batch_ext_set_output_embd(batch, idx, true);
    }
    *pos += 1;
}

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.to_str().ok_or("non-UTF-8 path")?).map_err(|e| e.to_string())
}

impl Drop for Embedder {
    fn drop(&mut self) {
        unsafe {
            sys::mtmd_free(self.mtmd);
            sys::llama_free(self.ctx);
            sys::llama_model_free(self.model);
        }
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
