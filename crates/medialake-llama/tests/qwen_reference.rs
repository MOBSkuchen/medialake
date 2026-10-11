//! Parity check against the usage example of the Qwen3-VL-Embedding-2B model card
//! (https://huggingface.co/Qwen/Qwen3-VL-Embedding-2B): same queries, text document and image,
//! similarities must match the published ones. Run after bumping llama.cpp:
//!   MEDIALAKE_MODEL_DIR=<dir with the 2B gguf files> MEDIALAKE_QWEN_DEMO_IMAGE=<demo.jpeg> \
//!     cargo test -p medialake-llama --test qwen_reference -- --ignored --nocapture
//! demo.jpeg: https://qianwen-res.oss-cn-beijing.aliyuncs.com/Qwen-VL/assets/demo.jpeg

use std::path::PathBuf;

use medialake_llama::{Embedder, Input, Params, Pooling, cosine};

const PREFIX: &str = "<|im_start|>system\nRepresent the user's input.<|im_end|>\n<|im_start|>user\n";
const SUFFIX: &str = "<|im_end|>\n<|im_start|>assistant\n";

const QUERIES: [&str; 4] = [
    "A woman playing with her dog on a beach at sunset.",
    "Pet owner training dog outdoors near water.",
    "Woman surfing on waves during a sunny day.",
    "City skyline view from a high-rise building at night.",
];
const DOCUMENT: &str = "A woman shares a joyful moment with her golden retriever on a sun-drenched beach at sunset, as the dog offers its paw in a heartwarming display of companionship and trust.";
/// Published similarities of each query to (text document, image).
const EXPECTED: [(f32, f32); 4] = [(0.8160, 0.7155), (0.5173, 0.3295), (0.3863, 0.2987), (0.1061, 0.0433)];

#[test]
#[ignore]
fn matches_model_card() {
    let dir = PathBuf::from(std::env::var_os("MEDIALAKE_MODEL_DIR").expect("MEDIALAKE_MODEL_DIR"));
    let image = std::fs::read(std::env::var_os("MEDIALAKE_QWEN_DEMO_IMAGE").expect("MEDIALAKE_QWEN_DEMO_IMAGE")).unwrap();
    let file = std::env::var("MEDIALAKE_QWEN_GGUF").unwrap_or_else(|_| "Qwen.Qwen3-VL-Embedding-2B.Q8_0.gguf".into());
    let params = Params { pooling: Pooling::Last, media_prefix: PREFIX.into(), media_suffix: SUFFIX.into() };
    let gpu = if cfg!(target_os = "macos") { 999 } else { 0 };
    let mut e = Embedder::load(&dir.join(file), &dir.join("mmproj-Qwen.Qwen3-VL-Embedding-2B.f16.gguf"), gpu, false, params).unwrap();
    assert_eq!(e.dims(), 2048);

    let text = |e: &mut Embedder, t: &str| e.embed(&Input::Text(&format!("{PREFIX}{t}{SUFFIX}"))).unwrap();
    let doc = text(&mut e, DOCUMENT);
    let img = e.embed(&Input::Images(&[&image])).unwrap();
    let mut worst = 0f32;
    for (q, (want_text, want_image)) in QUERIES.iter().zip(EXPECTED) {
        let v = text(&mut e, q);
        let (t, i) = (cosine(&v, &doc), cosine(&v, &img));
        println!("text {t:.4} (ref {want_text:.4})  image {i:.4} (ref {want_image:.4})  | {q}");
        worst = worst.max((t - want_text).abs()).max((i - want_image).abs());
    }
    // quantisation (Q8_0) and llama.cpp's image preprocessing shift the values slightly
    assert!(worst < 0.05, "largest deviation from the model card: {worst:.4}");
}
